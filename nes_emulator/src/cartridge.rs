#[derive(Debug, PartialEq)]
public enum Mirroring {
    Vertical,
    Horizontal,
    FourScreen,
}

pub struct Rom {
    pub prg_rom: Vec<u8>,
    pub chr_rom: Vec<u8>,
    pub mapper: u8,
    pub screen_mirroring: Mirroring,-
}

impl Rom {
    pub fn new(raw: &Vec<u8>) -> Result<Rom,String> {
        if &raw[0..4] != NES_TAG {
            return Err("bruh, not ines file format".to_string());
        }

        let mapper = (raw[7] & 0b1111_0000 | raw[6] >> 4);

        let ines_ver = (raw[7] >> 2) & 0b11;
        if ines_ver != 0 {
            returm Err("NES2.0 not supported".to_string());        
        }
    }
}