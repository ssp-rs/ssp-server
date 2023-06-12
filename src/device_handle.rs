#![allow(dead_code)]

use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time;

use parking_lot::{Mutex, MutexGuard};
use serialport::TTYPort;

use ssp::{CommandOps, MessageOps, ResponseOps, Result};

/// Timeout for waiting for lock on a mutex (milliseconds).
pub const LOCK_TIMEOUT_MS: u64 = 5_000;
/// Timeout for waiting for serial communication (milliseconds).
pub const SERIAL_TIMEOUT_MS: u64 = 10_000;
/// Minimum polling interval between messages (milliseconds).
pub const MIN_POLLING_MS: u64 = 200;
/// Maximum polling interval between messages (milliseconds).
#[allow(dead_code)]
pub const MAX_POLLING_MS: u64 = 1_000;
/// Default serial connection BAUD rate (bps).
pub const BAUD_RATE: u32 = 9_600;

pub(crate) static SEQ_FLAG: AtomicBool = AtomicBool::new(false);
static POLLING_INIT: AtomicBool = AtomicBool::new(false);

pub(crate) fn sequence_flag() -> ssp::SequenceFlag {
    SEQ_FLAG.load(Ordering::Relaxed).into()
}

pub(crate) fn set_sequence_flag(flag: ssp::SequenceFlag) {
    SEQ_FLAG.store(flag.into(), Ordering::SeqCst);
}

// Whether the polling routine has started.
fn polling_inited() -> bool {
    POLLING_INIT.load(Ordering::Relaxed)
}

// Sets the flag indicating whether the polling routine started.
fn set_polling_inited(inited: bool) {
    POLLING_INIT.store(inited, Ordering::SeqCst);
}

// Convenience macro to get a Option<&ssp::AesKey> from the device handle.
//
// If the encryption key is unset, returns `None`.
macro_rules! encryption_key {
    ($handle:tt) => {{
        $handle.encryption_key()?.as_ref()
    }};
}

macro_rules! continue_on_err {
    ($res:expr, $err:tt) => {{
        match $res {
            Ok(res) => res,
            Err(err) => {
                let err_msg = $err;
                log::warn!("{err_msg}: {err}");
                continue;
            }
        }
    }};
}

pub struct DeviceHandle {
    serial_port: Arc<Mutex<TTYPort>>,
    generator: ssp::GeneratorKey,
    modulus: ssp::ModulusKey,
    random: ssp::RandomKey,
    fixed_key: ssp::FixedKey,
    key: Arc<Mutex<Option<ssp::AesKey>>>,
}

impl DeviceHandle {
    /// Creates a new [DeviceHandle] with a serial connection over the supplied serial device.
    pub fn new(serial_path: &str) -> Result<Self> {
        // For details on the following setup, see sections 5.4 & 7 in the SSP implementation guide
        let serial_port = Arc::new(Mutex::new(
            serialport::new(serial_path, BAUD_RATE)
                // disable flow control serial lines
                .flow_control(serialport::FlowControl::None)
                // eight-bit data size
                .data_bits(serialport::DataBits::Eight)
                // no control bit parity
                .parity(serialport::Parity::None)
                // two bit stop
                .stop_bits(serialport::StopBits::Two)
                // serial device times out after 10 seconds, so do we
                .timeout(time::Duration::from_millis(SERIAL_TIMEOUT_MS))
                // get back a TTY port for POSIX systems, Windows is not supported
                .open_native()?,
        ));

        let mut prime_gen = ssp::primes::Generator::from_entropy();

        let generator = ssp::GeneratorKey::from_generator(&mut prime_gen);
        let mut modulus = ssp::ModulusKey::from_generator(&mut prime_gen);

        // Modulus key must be smaller than the Generator key
        while modulus.as_inner() >= generator.as_inner() {
            modulus = ssp::ModulusKey::from_generator(&mut prime_gen);
        }

        let random = ssp::RandomKey::from_entropy();
        let fixed_key = ssp::FixedKey::new();
        let key = Arc::new(Mutex::new(None));

        Ok(Self {
            serial_port,
            generator,
            modulus,
            random,
            fixed_key,
            key,
        })
    }

    /// Starts background polling routine to regularly send [PollCommand] messages to the device.
    ///
    /// **Args**
    ///
    /// - `stop_polling`: used to control when the polling routine should stop sending polling messages.
    ///
    /// If background polling has already started, the function just returns.
    pub fn start_background_polling(&self, stop_polling: Arc<AtomicBool>) -> Result<()> {
        if polling_inited() {
            Ok(())
        } else {
            // Set the global flag to disallow multiple background polling threads.
            set_polling_inited(true);

            let serial_port = Arc::clone(&self.serial_port);
            let end_polling = Arc::clone(&stop_polling);
            let key = Arc::clone(&self.key);

            thread::spawn(move || -> Result<()> {
                let now = time::Instant::now();

                while !end_polling.load(Ordering::Relaxed) {
                    if now.elapsed().as_millis() % MIN_POLLING_MS as u128 == 0 {
                        let mut locked_port = continue_on_err!(
                            Self::lock_serial_port(&serial_port),
                            "Failed to lock serial port in background polling routine"
                        );
                        let key = continue_on_err!(
                            Self::lock_encryption_key(&key),
                            "Failed to lock encryption key in background polling routine"
                        );

                        let mut message = ssp::PollCommand::new();

                        if let Some(key) = key.as_ref() {
                            match Self::poll_encrypted_message(&mut locked_port, &mut message, key)
                            {
                                Ok(response) => {
                                    let poll_res = continue_on_err!(response.into_poll_response(), "Failed to convert to poll response in background polling routine");
                                    let last_statuses = poll_res.last_response_statuses();

                                    log::debug!("Successful encrypted poll command, last statuses: {last_statuses}");
                                }
                                Err(err) => {
                                    log::warn!("Failed encrypted poll command: {err}");
                                }
                            }
                        } else {
                            let res = continue_on_err!(
                                Self::poll_message_variant(&mut locked_port, &mut message),
                                "Failed poll command in background polling routine"
                            );
                            let status = res.as_response().response_status();

                            if status.is_ok() {
                                let poll_res = continue_on_err!(
                                    res.into_poll_response(),
                                    "Failed to convert poll response in background polling routine"
                                );
                                let last_statuses = poll_res.last_response_statuses();

                                log::debug!(
                                    "Successful poll command, last statuses: {last_statuses}"
                                );
                            } else {
                                log::warn!("Failed poll command, response status: {status}");
                            }
                        }
                    }
                }

                // Now that polling finished, reset the flag to allow another background routine to
                // start.
                set_polling_inited(false);

                Ok(())
            });

            Ok(())
        }
    }

    /// Get the serial port used for communication with the acceptor device
    pub fn serial_port(&self) -> Result<MutexGuard<'_, TTYPort>> {
        Self::lock_serial_port(&self.serial_port)
    }

    pub(crate) fn lock_serial_port(
        serial_port: &Arc<Mutex<TTYPort>>,
    ) -> Result<MutexGuard<'_, TTYPort>> {
        serial_port
            .try_lock_for(time::Duration::from_millis(LOCK_TIMEOUT_MS))
            .ok_or(ssp::Error::SerialPort(serialport::ErrorKind::Io(
                std::io::ErrorKind::TimedOut,
            )))
    }

    /// Acquires a lock on the AES encryption key.
    pub fn encryption_key(&self) -> Result<MutexGuard<'_, Option<ssp::AesKey>>> {
        Self::lock_encryption_key(&self.key)
    }

    pub(crate) fn lock_encryption_key(
        key: &Arc<Mutex<Option<ssp::AesKey>>>,
    ) -> Result<MutexGuard<'_, Option<ssp::AesKey>>> {
        key.try_lock_for(time::Duration::from_millis(LOCK_TIMEOUT_MS))
            .ok_or(ssp::Error::Io(std::io::ErrorKind::TimedOut))
    }

    /// Creates a new [GeneratorKey](ssp::GeneratorKey) from system entropy.
    pub fn new_generator_key(&mut self) {
        self.generator = ssp::GeneratorKey::from_entropy();
        self.reset_key();
    }

    /// Creates a new [ModulusKey](ssp::ModulusKey) from system entropy.
    pub fn new_modulus_key(&mut self) {
        let mut modulus = ssp::ModulusKey::from_entropy();

        // Modulus key must be smaller than the Generator key
        while modulus.as_inner() >= self.generator.as_inner() {
            modulus = ssp::ModulusKey::from_entropy();
        }

        self.modulus = modulus;

        self.reset_key();
    }

    /// Creates a new [RandomKey](ssp::RandomKey) from system entropy.
    pub fn new_random_key(&mut self) {
        self.random = ssp::RandomKey::from_entropy();
        self.reset_key();
    }

    fn generator_key(&self) -> &ssp::GeneratorKey {
        &self.generator
    }

    fn modulus_key(&self) -> &ssp::ModulusKey {
        &self.modulus
    }

    fn random_key(&self) -> &ssp::RandomKey {
        &self.random
    }

    fn set_key(&mut self, inter_key: ssp::IntermediateKey) -> Result<()> {
        let mut key = self.encryption_key()?;

        let mut new_key = ssp::AesKey::from(&self.fixed_key);
        let enc_key =
            ssp::EncryptionKey::from_keys(&inter_key, self.random_key(), self.modulus_key());

        new_key[..8].copy_from_slice(enc_key.as_inner().to_le_bytes().as_ref());

        key.replace(new_key);

        Ok(())
    }

    // Resets the Encryption key to none, requires a new key negotiation before performing eSSP
    // operations.
    fn reset_key(&mut self) -> Option<ssp::AesKey> {
        if let Ok(mut key) = self.encryption_key() {
            key.take()
        } else {
            None
        }
    }

    /// Send a [SetInhibitsCommand](ssp::SetInhibitsCommand) message to the device.
    ///
    /// No response is returned.
    ///
    /// The caller should wait a reasonable amount of time for the device
    /// to come back online before sending additional messages.
    pub fn set_inhibits(
        &mut self,
        enable_list: ssp::EnableBitfieldList,
    ) -> Result<ssp::SetInhibitsResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SetInhibitsCommand::new();
        message.set_inhibits(enable_list)?;

        let res = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        res.into_set_inhibits_response()
    }

    /// Send a [ResetCommand](ssp::ResetCommand) message to the device.
    ///
    /// No response is returned.
    ///
    /// The caller should wait a reasonable amount of time for the device
    /// to come back online before sending additional messages.
    pub fn reset(&mut self) -> Result<()> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::ResetCommand::new();

        Self::set_message_sequence_flag(&mut message);

        serial_port.write_all(message.as_bytes())?;

        Ok(())
    }

    /// Send a [PollCommand](ssp::PollCommand) message to the device.
    pub fn poll(&mut self) -> Result<ssp::PollResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::PollCommand::new();

        Self::set_message_sequence_flag(&mut message);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_poll_response()
    }

    /// Send a [PollWithAckCommand](ssp::PollWithAckCommand) message to the device.
    pub fn poll_with_ack(&mut self) -> Result<ssp::PollWithAckResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::PollWithAckCommand::new();

        Self::set_message_sequence_flag(&mut message);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_poll_with_ack_response()
    }

    /// Send a [EventAckCommand](ssp::EventAckCommand) message to the device.
    pub fn event_ack(&mut self) -> Result<ssp::EventAckResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::EventAckCommand::new();

        Self::set_message_sequence_flag(&mut message);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_event_ack_response()
    }

    /// Send a [RejectCommand](ssp::RejectCommand) message to the device.
    pub fn reject(&mut self) -> Result<ssp::RejectResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::RejectCommand::new();

        Self::set_message_sequence_flag(&mut message);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_reject_response()
    }

    /// Send a [SyncCommand](ssp::SyncCommand) message to the device.
    pub fn sync(&mut self) -> Result<ssp::SyncResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SyncCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        // Ensure the next sequence flag sent is set.
        // FIXME: regardless of the setting, Sync messages appear to cause problems with following
        // messages. Need more hardware to troubleshoot.
        set_sequence_flag(ssp::SequenceFlag::Set);

        response.into_sync_response()
    }

    /// Send a [EnableCommand](ssp::EnableCommand) message to the device.
    pub fn enable(&mut self) -> Result<ssp::EnableResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::EnableCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_enable_response()
    }

    /// Send a [DisableCommand](ssp::DisableCommand) message to the device.
    pub fn disable(&mut self) -> Result<ssp::DisableResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::DisableCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_disable_response()
    }

    /// Send a [DisplayOffCommand](ssp::DisplayOffCommand) message to the device.
    pub fn display_off(&mut self) -> Result<ssp::DisplayOffResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::DisplayOffCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_display_off_response()
    }

    /// Send a [DisplayOnCommand](ssp::DisplayOnCommand) message to the device.
    pub fn display_on(&mut self) -> Result<ssp::DisplayOnResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::DisplayOnCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_display_on_response()
    }

    /// Send an [EmptyCommand](ssp::EmptyCommand) message to the device.
    pub fn empty(&mut self) -> Result<ssp::EmptyResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::EmptyCommand::new();

        if let Some(key) = (*self.encryption_key()?).as_ref() {
            let res = Self::poll_encrypted_message(&mut serial_port, &mut message, key)?;

            res.into_empty_response()
        } else {
            Err(ssp::Error::Encryption(ssp::ResponseStatus::KeyNotSet))
        }
    }

    /// Send an [SmartEmptyCommand](ssp::SmartEmptyCommand) message to the device.
    pub fn smart_empty(&mut self) -> Result<ssp::SmartEmptyResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SmartEmptyCommand::new();

        if let Some(key) = self.encryption_key()?.as_ref() {
            let res = Self::poll_encrypted_message(&mut serial_port, &mut message, key)?;

            res.into_smart_empty_response()
        } else {
            Err(ssp::Error::Encryption(ssp::ResponseStatus::KeyNotSet))
        }
    }

    /// Send an [HostProtocolVersionCommand](ssp::HostProtocolVersionCommand) message to the device.
    pub fn host_protocol_version(
        &mut self,
        protocol_version: ssp::ProtocolVersion,
    ) -> Result<ssp::HostProtocolVersionResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::HostProtocolVersionCommand::new();
        message.set_version(protocol_version);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_host_protocol_version_response()
    }

    /// Send a [SerialNumberCommand](ssp::SerialNumberCommand) message to the device.
    pub fn serial_number(&mut self) -> Result<ssp::SerialNumberResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SerialNumberCommand::new();

        let res = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        res.into_serial_number_response()
    }

    /// Send a [SetGeneratorCommand](ssp::SetGeneratorCommand) message to the device.
    ///
    /// If the response is an `Err(_)`, or the response status is not
    /// [RsponseStatus::Ok](ssp::ResponseStatus::Ok), the caller should call
    /// [new_generator_key](Self::new_generator_key), and try again.
    pub fn set_generator(&mut self) -> Result<ssp::SetGeneratorResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SetGeneratorCommand::new();
        message.set_generator(self.generator_key());

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_set_generator_response()
    }

    /// Send a [SetModulusCommand](ssp::SetModulusCommand) message to the device.
    ///
    /// If the response is an `Err(_)`, or the response status is not
    /// [RsponseStatus::Ok](ssp::ResponseStatus::Ok), the caller should call
    /// [new_modulus_key](Self::new_modulus_key), and try again.
    pub fn set_modulus(&mut self) -> Result<ssp::SetModulusResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SetModulusCommand::new();
        message.set_modulus(self.modulus_key());

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_set_modulus_response()
    }

    /// Send a [RequestKeyExchangeCommand](ssp::RequestKeyExchangeCommand) message to the device.
    ///
    /// If the response is an `Err(_)`, or the response status is not
    /// [RsponseStatus::Ok](ssp::ResponseStatus::Ok), the caller should call
    /// [new_random_key](Self::new_random_key), and try again.
    pub fn request_key_exchange(&mut self) -> Result<ssp::RequestKeyExchangeResponse> {
        let res = {
            let mut serial_port = self.serial_port()?;

            let mut message = ssp::RequestKeyExchangeCommand::new();

            let inter_key = ssp::IntermediateKey::from_keys(
                self.generator_key(),
                self.random_key(),
                self.modulus_key(),
            );
            message.set_intermediate_key(&inter_key);

            let response =
                Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

            response.into_request_key_exchange_response()?
        };

        // If the exchange was successful, set the new encryption key.
        if res.response_status().is_ok() {
            self.set_key(res.intermediate_key())?;
        }

        Ok(res)
    }

    /// Send a [SetEncryptionKeyCommand](ssp::SetEncryptionKeyCommand) message to the device.
    ///
    /// If the response is an `Err(_)`, or the response status is not
    /// [RsponseStatus::Ok](ssp::ResponseStatus::Ok), the caller should call
    /// [new_modulus_key](Self::new_modulus_key), and try again.
    pub fn set_encryption_key(&mut self) -> Result<ssp::SetEncryptionKeyResponse> {
        let mut message = ssp::SetEncryptionKeyCommand::new();

        let fixed_key = ssp::FixedKey::from_entropy();
        message.set_fixed_key(&fixed_key);

        let res = if let Some(key) = encryption_key!(self) {
            let mut serial_port = self.serial_port()?;
            Self::poll_encrypted_message(&mut serial_port, &mut message, key)
        } else {
            Err(ssp::Error::Encryption(ssp::ResponseStatus::KeyNotSet))
        };

        match res {
            Ok(m) => {
                self.fixed_key = fixed_key;
                m.into_set_encryption_key_response()
            }
            Err(err) => Err(err),
        }
    }

    /// Send a [EncryptionResetCommand](ssp::EncryptionResetCommand) message to the device.
    pub fn encryption_reset(&mut self) -> Result<ssp::EncryptionResetResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::EncryptionResetCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        if response.as_response().response_status() == ssp::ResponseStatus::CommandCannotBeProcessed
        {
            Err(ssp::Error::Encryption(
                ssp::ResponseStatus::CommandCannotBeProcessed,
            ))
        } else {
            response.into_encryption_reset_response()
        }
    }

    /// Send a [SetupRequestCommand](ssp::SetupRequestCommand) message to the device.
    pub fn setup_request(&mut self) -> Result<ssp::SetupRequestResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SetupRequestCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_setup_request_response()
    }

    /// Send a [UnitDataCommand](ssp::UnitDataCommand) message to the device.
    pub fn unit_data(&mut self) -> Result<ssp::UnitDataResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::UnitDataCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_unit_data_response()
    }

    /// Send a [ChannelValueDataCommand](ssp::ChannelValueDataCommand) message to the device.
    pub fn channel_value_data(&mut self) -> Result<ssp::ChannelValueDataResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::ChannelValueDataCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_channel_value_data_response()
    }

    /// Send a [LastRejectCodeCommand](ssp::LastRejectCodeCommand) message to the device.
    pub fn last_reject_code(&mut self) -> Result<ssp::LastRejectCodeResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::LastRejectCodeCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_last_reject_code_response()
    }

    /// Send a [HoldCommand](ssp::HoldCommand) message to the device.
    pub fn hold(&mut self) -> Result<ssp::HoldResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::HoldCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_hold_response()
    }

    /// Send a [GetBarcodeReaderConfigurationCommand](ssp::GetBarcodeReaderConfigurationCommand) message to the device.
    pub fn get_barcode_reader_configuration(
        &mut self,
    ) -> Result<ssp::GetBarcodeReaderConfigurationResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::GetBarcodeReaderConfigurationCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_get_barcode_reader_configuration_response()
    }

    /// Gets whether the device has barcode readers present.
    pub fn has_barcode_reader(&mut self) -> Result<bool> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::GetBarcodeReaderConfigurationCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        Ok(response
            .as_get_barcode_reader_configuration_response()?
            .hardware_status()
            != ssp::BarcodeHardwareStatus::None)
    }

    /// Send a [SetBarcodeReaderConfigurationCommand](ssp::SetBarcodeReaderConfigurationCommand) message to the device.
    pub fn set_barcode_reader_configuration(
        &mut self,
        config: ssp::BarcodeConfiguration,
    ) -> Result<ssp::SetBarcodeReaderConfigurationResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SetBarcodeReaderConfigurationCommand::new();
        message.set_configuration(config);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_set_barcode_reader_configuration_response()
    }

    /// Send a [GetBarcodeInhibitCommand](ssp::GetBarcodeInhibitCommand) message to the device.
    pub fn get_barcode_inhibit(&mut self) -> Result<ssp::GetBarcodeInhibitResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::GetBarcodeInhibitCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_get_barcode_inhibit_response()
    }

    /// Send a [SetBarcodeInhibitCommand](ssp::SetBarcodeInhibitCommand) message to the device.
    pub fn set_barcode_inhibit(
        &mut self,
        inhibit: ssp::BarcodeCurrencyInhibit,
    ) -> Result<ssp::SetBarcodeInhibitResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::SetBarcodeInhibitCommand::new();
        message.set_inhibit(inhibit);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_set_barcode_inhibit_response()
    }

    /// Send a [GetBarcodeDataCommand](ssp::GetBarcodeDataCommand) message to the device.
    pub fn get_barcode_data(&mut self) -> Result<ssp::GetBarcodeDataResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::GetBarcodeDataCommand::new();

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_get_barcode_data_response()
    }

    /// Send a [ConfigureBezelCommand](ssp::ConfigureBezelCommand) message to the device.
    pub fn configure_bezel(
        &mut self,
        rgb: ssp::RGB,
        storage: ssp::BezelConfigStorage,
    ) -> Result<ssp::ConfigureBezelResponse> {
        let mut serial_port = self.serial_port()?;

        let mut message = ssp::ConfigureBezelCommand::new();
        message.set_rgb(rgb);
        message.set_config_storage(storage);

        let response = Self::poll_message(&mut serial_port, &mut message, encryption_key!(self))?;

        response.into_configure_bezel_response()
    }

    fn set_message_sequence_flag(message: &mut dyn CommandOps) {
        let mut sequence_id = message.sequence_id();
        sequence_id.set_flag(sequence_flag());
        message.set_sequence_id(sequence_id);
    }

    fn poll_message_variant(
        serial_port: &mut TTYPort,
        message: &mut dyn CommandOps,
    ) -> Result<ssp::MessageVariant> {
        use ssp::message::index;

        Self::set_message_sequence_flag(message);

        log::trace!(
            "Message type: {}, SEQID: {}",
            message.message_type(),
            message.sequence_id()
        );

        let mut attempt = 0;
        while let Err(_err) = serial_port.write_all(message.as_bytes()) {
            attempt += 1;
            log::warn!("Failed to send message, attmept #{attempt}");

            thread::sleep(time::Duration::from_millis(MIN_POLLING_MS));

            message.toggle_sequence_id();
        }

        // Set the global sequence flag to the opposite value for the next message
        set_sequence_flag(!message.sequence_id().flag());

        let mut buf = [0u8; ssp::len::MAX_MESSAGE];

        serial_port.read_exact(buf[..index::SEQ_ID].as_mut())?;

        let stx = buf[index::STX];
        if stx != ssp::STX {
            return Err(ssp::Error::InvalidSTX(stx));
        }

        serial_port.read_exact(buf[index::SEQ_ID..=index::LEN].as_mut())?;

        let buf_len = buf[index::LEN] as usize;
        let remaining = index::DATA + buf_len + 2; // data + CRC-16 bytes
        let total = buf_len + ssp::len::METADATA;

        serial_port.read_exact(buf[index::DATA..remaining].as_mut())?;

        ssp::MessageVariant::from_buf(buf[..total].as_ref(), message.message_type())
    }

    fn poll_encrypted_message(
        serial_port: &mut TTYPort,
        message: &mut dyn CommandOps,
        key: &ssp::AesKey,
    ) -> Result<ssp::MessageVariant> {
        let mut enc_cmd = ssp::EncryptedCommand::new();
        enc_cmd.set_message_data(message)?;

        let mut wrapped = enc_cmd.encrypt(key);

        let response = Self::poll_message_variant(serial_port, &mut wrapped)?;

        if response.as_response().response_status() == ssp::ResponseStatus::KeyNotSet {
            return Err(ssp::Error::Encryption(ssp::ResponseStatus::KeyNotSet));
        }

        let wrapped_res = response.into_wrapped_encrypted_message()?;
        let dec_res = ssp::EncryptedResponse::decrypt(&key, wrapped_res);

        let mut res = ssp::MessageVariant::new(message.command());
        res.as_response_mut().set_data(dec_res.data())?;
        res.as_response_mut().calculate_checksum();

        Ok(res)
    }

    fn poll_message(
        serial_port: &mut TTYPort,
        message: &mut dyn CommandOps,
        key: Option<&ssp::AesKey>,
    ) -> Result<ssp::MessageVariant> {
        if let Some(key) = key {
            Self::poll_encrypted_message(serial_port, message, key)
        } else {
            Self::poll_message_variant(serial_port, message)
        }
    }
}
