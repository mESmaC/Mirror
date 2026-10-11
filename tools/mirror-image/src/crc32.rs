//! The CRC-32 that GPT uses (UEFI 2.10, appendix F): the IEEE 802.3
//! polynomial, reflected, with an initial value and final XOR of all ones.

const TABLE: [u32; 256] = table();

const fn table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

pub fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, byte| {
        TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}
