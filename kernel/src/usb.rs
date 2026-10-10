//! Allocation-free USB 2 descriptor validation and bounded boot-HID discovery.
//!
//! Descriptor bytes are untrusted device input. A structurally valid device may
//! still have an unsupported profile; neither result configures a USB device or
//! claims that input reports work. No vendor/product allowlist is used.
//!
//! USB 2.0 chapter 9 and HID 1.11 sections 4, 6 and appendix B:
//! https://www.usb.org/document-library/usb-20-specification
//! https://www.usb.org/sites/default/files/documents/hid1_11.pdf

pub const MAX_CONFIG_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbSpeed {
    Low,
    Full,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorError {
    Length,
    DescriptorType,
    UsbVersion,
    PacketSize,
    ConfigurationCount,
    ConfigurationTooLarge,
    ConfigurationChanged,
    ConfigurationValue,
    ConfigurationAttributes,
    InterfaceCount,
    InterfaceNumber,
    DuplicateInterface,
    MissingDefaultInterface,
    EndpointCount,
    EndpointWithoutInterface,
    EndpointAddress,
    EndpointAttributes,
    DuplicateEndpoint,
    EndpointInterval,
    HidDescriptor,
    UnexpectedDescriptor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb_version: u16,
    /// Raw vendor release value, used only for diagnostics, never as a size.
    pub device_version: u16,
    pub vendor: u16,
    pub product: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet0: u8,
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    pub fn boot_hid(
        self,
        configuration: &Configuration,
    ) -> Result<BootHidInterface, UnsupportedProfile> {
        if self.device_class == 9 {
            return Err(UnsupportedProfile::Hub);
        }
        if self.device_class != 0 || self.device_subclass != 0 || self.device_protocol != 0 {
            return Err(UnsupportedProfile::DeviceClass);
        }
        if self.num_configurations != 1 {
            return Err(UnsupportedProfile::MultipleConfigurations);
        }
        configuration.profile
    }
}

/// Validate the actual eight-byte prefix before changing endpoint-zero context.
pub fn device_prefix(bytes: &[u8], speed: UsbSpeed) -> Result<u8, DescriptorError> {
    if bytes.len() != 8 {
        return Err(DescriptorError::Length);
    }
    if bytes[0] != 18 || bytes[1] != 1 {
        return Err(DescriptorError::DescriptorType);
    }
    let version = word(bytes, 2);
    if !(0x0100..=0x0299).contains(&version)
        || !is_bcd(version)
        || (speed == UsbSpeed::High && version < 0x0200)
    {
        return Err(DescriptorError::UsbVersion);
    }
    if !control_packet(speed, u16::from(bytes[7])) {
        return Err(DescriptorError::PacketSize);
    }
    Ok(bytes[7])
}

pub fn parse_device(bytes: &[u8], speed: UsbSpeed) -> Result<DeviceDescriptor, DescriptorError> {
    if bytes.len() != 18 {
        return Err(DescriptorError::Length);
    }
    let max_packet0 = device_prefix(&bytes[..8], speed)?;
    if bytes[17] == 0 {
        return Err(DescriptorError::ConfigurationCount);
    }
    Ok(DeviceDescriptor {
        usb_version: word(bytes, 2),
        device_version: word(bytes, 12),
        vendor: word(bytes, 8),
        product: word(bytes, 10),
        device_class: bytes[4],
        device_subclass: bytes[5],
        device_protocol: bytes[6],
        max_packet0,
        num_configurations: bytes[17],
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigurationHeader {
    pub total_length: u16,
    pub value: u8,
    pub interfaces: u8,
}

/// Use only an actual nine-byte header, not a padded transfer buffer.
pub fn configuration_header(bytes: &[u8]) -> Result<ConfigurationHeader, DescriptorError> {
    if bytes.len() != 9 {
        return Err(DescriptorError::Length);
    }
    if bytes[0] != 9 || bytes[1] != 2 {
        return Err(DescriptorError::DescriptorType);
    }
    let total_length = word(bytes, 2);
    if total_length < 9 {
        return Err(DescriptorError::Length);
    }
    if usize::from(total_length) > MAX_CONFIG_BYTES {
        return Err(DescriptorError::ConfigurationTooLarge);
    }
    if bytes[4] == 0 {
        return Err(DescriptorError::InterfaceCount);
    }
    if bytes[5] == 0 {
        return Err(DescriptorError::ConfigurationValue);
    }
    if bytes[7] & 0x9f != 0x80 {
        return Err(DescriptorError::ConfigurationAttributes);
    }
    Ok(ConfigurationHeader {
        total_length,
        value: bytes[5],
        interfaces: bytes[4],
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedProfile {
    DeviceClass,
    Hub,
    MultipleConfigurations,
    Composite,
    AlternateSettings,
    InterfaceClass,
    NonBootHid,
    HidProtocol,
    MissingHidDescriptor,
    ExtraDescriptors,
    EndpointLayout,
    PollingInterval,
    HighBandwidthEndpoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootHidInterface {
    pub configuration: u8,
    pub interface: u8,
    /// HID boot protocol: 1 keyboard, 2 mouse. Reports are not enabled here.
    pub protocol: u8,
    pub endpoint: u8,
    pub max_packet: u16,
    pub interval: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub total_length: u16,
    pub value: u8,
    pub interfaces: u8,
    pub profile: Result<BootHidInterface, UnsupportedProfile>,
}

#[derive(Clone, Copy)]
struct Endpoint {
    address: u8,
    packet: u16,
    interval: u8,
    transfer: u8,
    transactions: u8,
}

struct Interface {
    number: u8,
    alternate: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    expected: u8,
    observed: u8,
    endpoints: u32,
    input: Option<Endpoint>,
    hid: bool,
    extra: bool,
}

impl Interface {
    fn finish(
        &self,
        configuration: u8,
        speed: UsbSpeed,
    ) -> Result<Result<BootHidInterface, UnsupportedProfile>, DescriptorError> {
        if self.observed != self.expected {
            return Err(DescriptorError::EndpointCount);
        }
        let profile = if self.class == 9 {
            Err(UnsupportedProfile::Hub)
        } else if self.class != 3 {
            Err(UnsupportedProfile::InterfaceClass)
        } else if self.subclass != 1 {
            Err(UnsupportedProfile::NonBootHid)
        } else if !matches!(self.protocol, 1 | 2) {
            Err(UnsupportedProfile::HidProtocol)
        } else if !self.hid {
            Err(UnsupportedProfile::MissingHidDescriptor)
        } else if self.extra {
            Err(UnsupportedProfile::ExtraDescriptors)
        } else if self.alternate != 0 {
            Err(UnsupportedProfile::AlternateSettings)
        } else if self.expected != 1 {
            Err(UnsupportedProfile::EndpointLayout)
        } else if let Some(endpoint) = self.input {
            let minimum_packet = if self.protocol == 1 { 8 } else { 3 };
            if endpoint.packet < minimum_packet {
                Err(UnsupportedProfile::EndpointLayout)
            } else if speed == UsbSpeed::Low && endpoint.interval < 10 {
                // Table 9-13 permits this descriptor encoding, while section
                // 5.7.4 limits low-speed requested polling periods to 10..255ms.
                Err(UnsupportedProfile::PollingInterval)
            } else if endpoint.transactions != 0 {
                Err(UnsupportedProfile::HighBandwidthEndpoint)
            } else {
                Ok(BootHidInterface {
                    configuration,
                    interface: self.number,
                    protocol: self.protocol,
                    endpoint: endpoint.address,
                    max_packet: endpoint.packet,
                    interval: endpoint.interval,
                })
            }
        } else {
            Err(UnsupportedProfile::EndpointLayout)
        };
        Ok(profile)
    }
}

/// Validate the whole first configuration and the previously read total length.
/// Alternate settings and composite devices are parsed safely but not selected.
pub fn parse_configuration(
    bytes: &[u8],
    expected_total: u16,
    speed: UsbSpeed,
) -> Result<Configuration, DescriptorError> {
    if bytes.len() < 9 {
        return Err(DescriptorError::Length);
    }
    let header = configuration_header(&bytes[..9])?;
    if header.total_length != expected_total {
        return Err(DescriptorError::ConfigurationChanged);
    }
    if bytes.len() != usize::from(header.total_length) {
        return Err(DescriptorError::Length);
    }
    let mut offset = 9;
    let mut current: Option<Interface> = None;
    let mut interfaces = [0_u64; 4];
    let mut defaults = [0_u64; 4];
    // Address owners span alternate settings, while each setting has its own
    // duplicate mask. An address cannot belong to two simultaneous interfaces.
    let mut owners = [0_u16; 32];
    let mut alternate = false;
    let mut association = false;
    let mut profile = Err(UnsupportedProfile::InterfaceClass);
    while offset < bytes.len() {
        if bytes.len() - offset < 2 {
            return Err(DescriptorError::Length);
        }
        let length = usize::from(bytes[offset]);
        if length < 2 || length > bytes.len() - offset {
            return Err(DescriptorError::Length);
        }
        let descriptor = &bytes[offset..offset + length];
        match descriptor[1] {
            4 => {
                if length != 9 {
                    return Err(DescriptorError::Length);
                }
                if let Some(interface) = current.take() {
                    profile = interface.finish(header.value, speed)?;
                }
                let number = descriptor[2];
                let setting = descriptor[3];
                if number >= header.interfaces {
                    return Err(DescriptorError::InterfaceNumber);
                }
                if prior_interface(&bytes[..offset], number, setting) {
                    return Err(DescriptorError::DuplicateInterface);
                }
                if descriptor[4] > 30 {
                    return Err(DescriptorError::EndpointCount);
                }
                let index = usize::from(number / 64);
                let bit = 1_u64 << (number % 64);
                interfaces[index] |= bit;
                if setting == 0 {
                    defaults[index] |= bit;
                } else {
                    alternate = true;
                }
                current = Some(Interface {
                    number,
                    alternate: setting,
                    class: descriptor[5],
                    subclass: descriptor[6],
                    protocol: descriptor[7],
                    expected: descriptor[4],
                    observed: 0,
                    endpoints: 0,
                    input: None,
                    hid: false,
                    extra: false,
                });
            }
            5 => {
                let interface = current
                    .as_mut()
                    .ok_or(DescriptorError::EndpointWithoutInterface)?;
                let endpoint = parse_endpoint(descriptor, speed, interface.alternate)?;
                let number = endpoint.address & 15;
                let index = usize::from(number + if endpoint.address & 0x80 != 0 { 16 } else { 0 });
                let mask = if endpoint.transfer == 0 {
                    (1 << number) | (1 << (number + 16))
                } else {
                    1 << index
                };
                if interface.endpoints & mask != 0 {
                    return Err(DescriptorError::DuplicateEndpoint);
                }
                for (position, owner) in owners.iter_mut().enumerate() {
                    if mask & (1 << position) == 0 {
                        continue;
                    }
                    if *owner != 0 && *owner != u16::from(interface.number) + 1 {
                        return Err(DescriptorError::DuplicateEndpoint);
                    }
                    *owner = u16::from(interface.number) + 1;
                }
                interface.endpoints |= mask;
                interface.observed += 1;
                if interface.observed > interface.expected {
                    return Err(DescriptorError::EndpointCount);
                }
                if endpoint.transfer == 3 && endpoint.address & 0x80 != 0 {
                    interface.input = Some(endpoint);
                }
            }
            0x21 if current
                .as_ref()
                .is_some_and(|interface| interface.class == 3) =>
            {
                let interface = current.as_mut().unwrap();
                if interface.hid || interface.observed != 0 {
                    return Err(DescriptorError::HidDescriptor);
                }
                interface.extra |= validate_hid(descriptor)?;
                interface.hid = true;
            }
            11 => {
                if length != 8
                    || descriptor[3] == 0
                    || u16::from(descriptor[2]) + u16::from(descriptor[3])
                        > u16::from(header.interfaces)
                {
                    return Err(DescriptorError::InterfaceCount);
                }
                association = true;
            }
            1..=3 | 6..=8 => return Err(DescriptorError::UnexpectedDescriptor),
            _ => {
                if let Some(interface) = current.as_mut() {
                    interface.extra = true;
                } else {
                    return Err(DescriptorError::UnexpectedDescriptor);
                }
            }
        }
        offset += length;
    }
    if let Some(interface) = current {
        profile = interface.finish(header.value, speed)?;
    }
    if interfaces.iter().map(|bits| bits.count_ones()).sum::<u32>() != u32::from(header.interfaces)
    {
        return Err(DescriptorError::InterfaceCount);
    }
    if defaults != interfaces {
        return Err(DescriptorError::MissingDefaultInterface);
    }
    if header.interfaces != 1 || association {
        profile = Err(UnsupportedProfile::Composite);
    } else if alternate {
        profile = Err(UnsupportedProfile::AlternateSettings);
    }
    Ok(Configuration {
        total_length: header.total_length,
        value: header.value,
        interfaces: header.interfaces,
        profile,
    })
}

fn prior_interface(prefix: &[u8], number: u8, alternate: u8) -> bool {
    let mut offset = 9;
    while offset < prefix.len() {
        // Only previously validated descriptors are walked here.
        if prefix[offset + 1] == 4
            && prefix[offset + 2] == number
            && prefix[offset + 3] == alternate
        {
            return true;
        }
        offset += usize::from(prefix[offset]);
    }
    false
}

fn parse_endpoint(
    bytes: &[u8],
    speed: UsbSpeed,
    alternate: u8,
) -> Result<Endpoint, DescriptorError> {
    if bytes.len() < 7 {
        return Err(DescriptorError::Length);
    }
    let transfer = bytes[3] & 3;
    if bytes.len() != 7 && !(bytes.len() == 9 && transfer == 1) {
        return Err(DescriptorError::Length);
    }
    if bytes[2] & 0x70 != 0 || bytes[2] & 15 == 0 {
        return Err(DescriptorError::EndpointAddress);
    }
    if (transfer == 1 && (bytes[3] & 0xc0 != 0 || bytes[3] & 0x30 == 0x30))
        || (transfer != 1 && bytes[3] & !3 != 0)
    {
        return Err(DescriptorError::EndpointAttributes);
    }
    let raw = word(bytes, 4);
    let packet = raw & 0x7ff;
    let transactions = ((raw >> 11) & 3) as u8;
    if raw & 0xe000 != 0
        || transactions == 3
        || (transactions != 0 && (speed != UsbSpeed::High || !matches!(transfer, 1 | 3)))
    {
        return Err(DescriptorError::PacketSize);
    }
    let valid_packet = match transfer {
        0 => control_packet(speed, packet),
        1 => {
            (match speed {
                UsbSpeed::Low => false,
                UsbSpeed::Full => packet <= 1023,
                UsbSpeed::High => packet <= 1024,
            }) && (packet != 0 || alternate == 0)
        }
        2 => match speed {
            UsbSpeed::Low => false,
            UsbSpeed::Full => matches!(packet, 8 | 16 | 32 | 64),
            UsbSpeed::High => packet == 512,
        },
        3 => {
            packet != 0
                && packet
                    <= match speed {
                        UsbSpeed::Low => 8,
                        UsbSpeed::Full => 64,
                        UsbSpeed::High => 1024,
                    }
                // USB2 section 5.7.3 reserves larger high-speed interrupt
                // packets for nondefault alternate settings.
                && (speed != UsbSpeed::High || alternate != 0 || packet <= 64)
        }
        _ => unreachable!(),
    };
    if !valid_packet {
        return Err(DescriptorError::PacketSize);
    }
    if (transfer == 1 && !(1..=16).contains(&bytes[6]))
        || (transfer == 3 && (bytes[6] == 0 || (speed == UsbSpeed::High && bytes[6] > 16)))
    {
        return Err(DescriptorError::EndpointInterval);
    }
    Ok(Endpoint {
        address: bytes[2],
        packet,
        interval: bytes[6],
        transfer,
        transactions,
    })
}

/// Return whether a well-formed HID descriptor names extensions beyond its one
/// report descriptor. The report bytes are not fetched or interpreted here.
fn validate_hid(bytes: &[u8]) -> Result<bool, DescriptorError> {
    // bcdHID does not select a memory layout here. For example, QEMU's mouse
    // reports 0001h while its keyboard reports 0111h. Accept nonzero BCD
    // metadata; the checked layout and boot interface fields select support.
    if bytes.len() < 9
        || bytes[5] == 0
        || bytes.len() != 6 + usize::from(bytes[5]) * 3
        || word(bytes, 2) == 0
        || !is_bcd(word(bytes, 2))
    {
        return Err(DescriptorError::HidDescriptor);
    }
    let mut report = false;
    let mut extra = false;
    for entry in bytes[6..].as_chunks::<3>().0 {
        if entry[0] == 0x22 {
            if report || word(entry, 1) == 0 {
                return Err(DescriptorError::HidDescriptor);
            }
            report = true;
        } else {
            extra = true;
        }
    }
    if !report {
        return Err(DescriptorError::HidDescriptor);
    }
    Ok(extra)
}

fn control_packet(speed: UsbSpeed, packet: u16) -> bool {
    match speed {
        UsbSpeed::Low => packet == 8,
        UsbSpeed::Full => matches!(packet, 8 | 16 | 32 | 64),
        UsbSpeed::High => packet == 64,
    }
}

fn is_bcd(value: u16) -> bool {
    (0..4).all(|index| (value >> (index * 4)) & 15 <= 9)
}
fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    let device = [18, 1, 0, 2, 0, 0, 0, 64, 0x27, 6, 1, 0, 0, 1, 1, 2, 3, 1];
    let keyboard = [
        9, 2, 34, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 1, 0, 1, 0x22,
        63, 0, 7, 5, 0x81, 3, 8, 0, 7,
    ];
    let identity = parse_device(&device, UsbSpeed::High).unwrap();
    assert_eq!(
        (identity.vendor, identity.product, identity.max_packet0),
        (0x0627, 1, 64)
    );
    assert_eq!(device_prefix(&device[..8], UsbSpeed::High), Ok(64));
    assert!(device_prefix(&device[..7], UsbSpeed::High).is_err());
    assert!(parse_device(&device[..17], UsbSpeed::High).is_err());
    assert!(parse_device(&device, UsbSpeed::Low).is_err());
    let mut low = device;
    low[7] = 8;
    assert!(parse_device(&low, UsbSpeed::Low).is_ok());
    for packet in [8, 16, 32, 64] {
        low[7] = packet;
        assert!(parse_device(&low, UsbSpeed::Full).is_ok());
    }
    for (index, value) in [(0, 17), (1, 2), (2, 0xfa), (3, 1), (3, 3), (7, 9), (17, 0)] {
        let mut invalid = device;
        invalid[index] = value;
        assert!(parse_device(&invalid, UsbSpeed::High).is_err());
    }
    let header = configuration_header(&keyboard[..9]).unwrap();
    assert_eq!(header.total_length, 34);
    assert!(configuration_header(&keyboard[..8]).is_err());
    let config = parse_configuration(&keyboard, header.total_length, UsbSpeed::High).unwrap();
    let hid = identity.boot_hid(&config).unwrap();
    assert_eq!(
        (
            hid.configuration,
            hid.interface,
            hid.protocol,
            hid.endpoint,
            hid.max_packet,
            hid.interval
        ),
        (1, 0, 1, 0x81, 8, 7)
    );
    let mut mouse = keyboard;
    // Actual QEMU USB2 mouse wire values, including its 0001h bcdHID.
    mouse[6] = 6;
    mouse[7] = 0xa0;
    mouse[16] = 2;
    mouse[20] = 1;
    mouse[21] = 0;
    mouse[25] = 52;
    mouse[31] = 4;
    assert_eq!(
        parse_configuration(&mouse, 34, UsbSpeed::High)
            .unwrap()
            .profile
            .unwrap()
            .protocol,
        2
    );
    for version in [0, 0x000a, 0x0a00] {
        let mut invalid = mouse;
        invalid[20..22].copy_from_slice(&u16::to_le_bytes(version));
        assert_eq!(
            parse_configuration(&invalid, 34, UsbSpeed::High),
            Err(DescriptorError::HidDescriptor)
        );
    }
    for interval in [1, 9] {
        let mut low_polling = keyboard;
        low_polling[33] = interval;
        assert_eq!(
            parse_configuration(&low_polling, 34, UsbSpeed::Low)
                .unwrap()
                .profile,
            Err(UnsupportedProfile::PollingInterval)
        );
    }
    for interval in [10, 255] {
        let mut low_polling = keyboard;
        low_polling[33] = interval;
        assert!(
            parse_configuration(&low_polling, 34, UsbSpeed::Low)
                .unwrap()
                .profile
                .is_ok()
        );
    }
    assert_eq!(
        parse_configuration(&mouse, 34, UsbSpeed::Full)
            .unwrap()
            .profile
            .unwrap()
            .protocol,
        2
    );
    for (protocol, packet) in [(1, 7), (2, 2)] {
        let mut small = keyboard;
        small[16] = protocol;
        small[31] = packet;
        assert_eq!(
            parse_configuration(&small, 34, UsbSpeed::High)
                .unwrap()
                .profile,
            Err(UnsupportedProfile::EndpointLayout)
        );
    }
    assert!(parse_configuration(&keyboard[..33], 34, UsbSpeed::High).is_err());
    assert_eq!(
        parse_configuration(&keyboard, 35, UsbSpeed::High),
        Err(DescriptorError::ConfigurationChanged)
    );
    for (index, value) in [
        (0, 8),
        (1, 1),
        (4, 0),
        (5, 0),
        (7, 0),
        (7, 0x81),
        (9, 0),
        (9, 1),
        (9, 8),
        (11, 1),
        (13, 0),
        (13, 2),
        (18, 8),
        (23, 0),
        (24, 0x23),
        (25, 0),
        (27, 6),
        (27, 8),
        (29, 0),
        (29, 0x80),
        (29, 0x91),
        (30, 0x83),
        (31, 0),
        (32, 0xe0),
        (33, 0),
        (33, 17),
    ] {
        let mut invalid = keyboard;
        invalid[index] = value;
        assert!(parse_configuration(&invalid, 34, UsbSpeed::High).is_err());
    }
    let mut oversized = keyboard;
    oversized[2] = 1;
    oversized[3] = 2;
    assert_eq!(
        configuration_header(&oversized[..9]),
        Err(DescriptorError::ConfigurationTooLarge)
    );
    let mut missing_default = keyboard;
    missing_default[12] = 1;
    assert_eq!(
        parse_configuration(&missing_default, 34, UsbSpeed::High),
        Err(DescriptorError::MissingDefaultInterface)
    );

    let mut pair = [0_u8; 59];
    pair[..34].copy_from_slice(&keyboard);
    pair[34..].copy_from_slice(&keyboard[9..]);
    pair[2] = 59;
    assert_eq!(
        parse_configuration(&pair, 59, UsbSpeed::High),
        Err(DescriptorError::DuplicateInterface)
    );
    pair[37] = 1; // Same interface, distinct alternate: endpoint reuse is valid.
    assert_eq!(
        parse_configuration(&pair, 59, UsbSpeed::High)
            .unwrap()
            .profile,
        Err(UnsupportedProfile::AlternateSettings)
    );
    pair[56] = 0;
    pair[57] = 4; // 1024-byte interrupt packet is legal only in nondefault alt1.
    assert_eq!(
        parse_configuration(&pair, 59, UsbSpeed::High)
            .unwrap()
            .profile,
        Err(UnsupportedProfile::AlternateSettings)
    );
    pair[56] = 8;
    pair[57] = 0;
    pair[37] = 0;
    pair[4] = 2;
    pair[36] = 1;
    assert_eq!(
        parse_configuration(&pair, 59, UsbSpeed::High),
        Err(DescriptorError::DuplicateEndpoint)
    );
    pair[54] = 0x82;
    assert_eq!(
        parse_configuration(&pair, 59, UsbSpeed::High)
            .unwrap()
            .profile,
        Err(UnsupportedProfile::Composite)
    );

    let mut endpoints = [0_u8; 41];
    endpoints[..34].copy_from_slice(&keyboard);
    endpoints[34..].copy_from_slice(&keyboard[27..]);
    endpoints[2] = 41;
    endpoints[13] = 2;
    assert_eq!(
        parse_configuration(&endpoints, 41, UsbSpeed::High),
        Err(DescriptorError::DuplicateEndpoint)
    );
    endpoints[36] = 0x82;
    assert_eq!(
        parse_configuration(&endpoints, 41, UsbSpeed::High)
            .unwrap()
            .profile,
        Err(UnsupportedProfile::EndpointLayout)
    );

    let mut no_hid = [0_u8; 25];
    no_hid[..18].copy_from_slice(&keyboard[..18]);
    no_hid[18..].copy_from_slice(&keyboard[27..]);
    no_hid[2] = 25;
    assert_eq!(
        parse_configuration(&no_hid, 25, UsbSpeed::High)
            .unwrap()
            .profile,
        Err(UnsupportedProfile::MissingHidDescriptor)
    );
    for (index, value, reason) in [
        (15, 0, UnsupportedProfile::NonBootHid),
        (16, 0, UnsupportedProfile::HidProtocol),
        (14, 0xff, UnsupportedProfile::InterfaceClass),
    ] {
        let mut unsupported = keyboard;
        unsupported[index] = value;
        assert_eq!(
            parse_configuration(&unsupported, 34, UsbSpeed::High)
                .unwrap()
                .profile,
            Err(reason)
        );
    }
    let mut hub = identity;
    hub.device_class = 9;
    assert_eq!(hub.boot_hid(&config), Err(UnsupportedProfile::Hub));
    hub.device_class = 0xef;
    assert_eq!(hub.boot_hid(&config), Err(UnsupportedProfile::DeviceClass));
    hub.device_class = 0;
    hub.num_configurations = 2;
    assert_eq!(
        hub.boot_hid(&config),
        Err(UnsupportedProfile::MultipleConfigurations)
    );

    let mut maximum = [0_u8; MAX_CONFIG_BYTES];
    maximum[..34].copy_from_slice(&keyboard);
    maximum[2] = 0;
    maximum[3] = 2;
    maximum[34] = 255;
    maximum[35] = 0x30;
    maximum[289] = 223;
    maximum[290] = 0x30;
    assert_eq!(
        parse_configuration(&maximum, 512, UsbSpeed::High)
            .unwrap()
            .profile,
        Err(UnsupportedProfile::ExtraDescriptors)
    );
    maximum[289] = 222; // One trailing byte is never treated as a descriptor.
    assert!(parse_configuration(&maximum, 512, UsbSpeed::High).is_err());

    let mut endpoint = [7, 5, 0x81, 3, 8, 0, 1];
    assert!(parse_endpoint(&endpoint, UsbSpeed::Low, 0).is_ok());
    endpoint[4] = 9;
    assert!(parse_endpoint(&endpoint, UsbSpeed::Low, 0).is_err());
    endpoint[4] = 65;
    assert!(parse_endpoint(&endpoint, UsbSpeed::Full, 0).is_err());
    endpoint[4] = 0;
    endpoint[5] = 4;
    assert!(parse_endpoint(&endpoint, UsbSpeed::High, 0).is_err());
    assert!(parse_endpoint(&endpoint, UsbSpeed::High, 1).is_ok());
    endpoint[4] = 1;
    assert!(parse_endpoint(&endpoint, UsbSpeed::High, 0).is_err());
    endpoint[4] = 8;
    endpoint[5] = 0x18;
    assert!(parse_endpoint(&endpoint, UsbSpeed::High, 0).is_err());
    let invalid_hid = [12, 0x21, 0x11, 1, 0, 2, 0x22, 63, 0, 0x22, 63, 0];
    assert!(validate_hid(&invalid_hid).is_err());
    crate::serial::println!("MIRROR_USB_TESTS_OK");
}
