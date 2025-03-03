use std::{fmt::Debug, sync::Arc};

use fxprof_processed_profile::{Symbol, SymbolTable};
use read::elf::NoteHeader;
use wholesym::samply_symbols::object::{elf, read, NativeEndian};

#[derive(Debug, Clone)]
pub struct KernelSymbols {
    pub build_id: Vec<u8>,
    pub base_avma: u64,
    pub symbol_table: Arc<SymbolTable>,
}

impl KernelSymbols {
    pub fn new_for_running_kernel() -> Option<Self> {
        let notes = std::fs::read("/sys/kernel/notes").ok()?;
        let build_id = build_id_from_notes_section_data(&notes)?.to_owned();
        let kallsyms = std::fs::read("/proc/kallsyms").ok()?;
        let (base_avma, symbol_table) = parse_kallsyms(&kallsyms)?;
        let symbol_table = Arc::new(symbol_table);
        Some(KernelSymbols {
            build_id,
            base_avma,
            symbol_table,
        })
    }
}

pub fn build_id_from_notes_section_data(section_data: &[u8]) -> Option<&[u8]> {
    let note_iter =
        NoteIterator::<elf::FileHeader64<NativeEndian>>::new(NativeEndian, 4, section_data)?;
    for note in note_iter {
        if note.name() == elf::ELF_NOTE_GNU && note.n_type(NativeEndian) == elf::NT_GNU_BUILD_ID {
            return Some(note.desc());
        }
    }
    None
}

struct KallSymIter<'a> {
    remaining_data: &'a [u8],
}

impl<'a> KallSymIter<'a> {
    pub fn new(proc_kallsyms: &'a [u8]) -> Self {
        Self {
            remaining_data: proc_kallsyms,
        }
    }
}

impl<'a> Iterator for KallSymIter<'a> {
    type Item = (u64, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_data.is_empty() {
            return None;
        }

        // Format: <hex address> <space> <letter> <space> <name> \n
        let (after_address, address) = hex_str::<u64>(self.remaining_data).ok()?;
        let starting_with_name = after_address.get(3..)?; // Skip <space> <letter> <space>
        match memchr::memchr(b'\n', starting_with_name) {
            Some(name_len) => {
                self.remaining_data = &starting_with_name[(name_len + 1)..];
                Some((address, &starting_with_name[..name_len]))
            }
            None => {
                self.remaining_data = &[];
                Some((address, starting_with_name))
            }
        }
    }
}

pub fn parse_kallsyms(data: &[u8]) -> Option<(u64, SymbolTable)> {
    let mut symbols = Vec::new();

    let mut text_addr = None;
    for (absolute_addr, symbol_name) in KallSymIter::new(data) {
        match (text_addr, symbol_name) {
            (None, b"_text") => {
                text_addr = Some(absolute_addr);
                symbols.push(Symbol {
                    address: 0,
                    size: None,
                    name: "_text".to_string(),
                });
            }
            (Some(text_addr), _) => {
                use std::convert::TryFrom;
                symbols.push(Symbol {
                    address: u32::try_from(absolute_addr - text_addr).ok()?,
                    size: None,
                    name: String::from_utf8_lossy(symbol_name).to_string(),
                });
            }
            (None, _) => {
                // Ignore symbols before the _text symbol.
            }
        }
    }
    Some((text_addr?, SymbolTable::new(symbols)))
}

/// Match a hex string, parse it to a u32 or a u64.
fn hex_str<T: std::ops::Shl<T, Output = T> + std::ops::BitOr<T, Output = T> + From<u8>>(
    input: &[u8],
) -> Result<(&[u8], T), &'static str> {
    // Consume up to max_len digits. For u32 that's 8 digits and for u64 that's 16 digits.
    // Two hex digits form one byte.
    let max_len = std::mem::size_of::<T>() * 2;

    let mut res: T = T::from(0);
    let mut k = 0;
    for v in input.iter().take(max_len) {
        let digit = match (*v as char).to_digit(16) {
            Some(v) => v,
            None => break,
        };
        res = res << T::from(4);
        res = res | T::from(digit as u8);
        k += 1;
    }
    if k == 0 {
        return Err("Bad hex digit");
    }
    let remaining = &input[k..];
    Ok((remaining, res))
}

/// An iterator over the notes in an ELF section or segment.
#[derive(Debug)]
pub struct NoteIterator<'data, Elf>
where
    Elf: read::elf::FileHeader,
{
    endian: Elf::Endian,
    align: usize,
    data: read::Bytes<'data>,
}

impl<'data, Elf> NoteIterator<'data, Elf>
where
    Elf: read::elf::FileHeader,
{
    /// Returns `Err` if `align` is invalid.
    pub(super) fn new(endian: Elf::Endian, align: Elf::Word, data: &'data [u8]) -> Option<Self> {
        let align = match align.into() {
            0u64..=4 => 4,
            8 => 8,
            _ => return None,
        };
        // TODO: check data alignment?
        Some(NoteIterator {
            endian,
            align,
            data: read::Bytes(data),
        })
    }
}

impl<'data, Elf> Iterator for NoteIterator<'data, Elf>
where
    Elf: read::elf::FileHeader,
{
    type Item = Note<'data, Elf>;
    /// Returns the next note.
    fn next(&mut self) -> Option<Note<'data, Elf>> {
        let mut data = self.data;
        if data.is_empty() {
            return None;
        }

        let header = data.read_at::<Elf::NoteHeader>(0).ok()?;

        // The name has no alignment requirement.
        let offset = std::mem::size_of::<Elf::NoteHeader>();
        let namesz = header.n_namesz(self.endian) as usize;
        let name = data.read_bytes_at(offset, namesz).ok()?.0;

        // The descriptor must be aligned.
        let offset = align(offset + namesz, self.align);
        let descsz = header.n_descsz(self.endian) as usize;
        let desc = data.read_bytes_at(offset, descsz).ok()?.0;

        // The next note (if any) must be aligned.
        let offset = align(offset + descsz, self.align);
        if data.skip(offset).is_err() {
            data = read::Bytes(&[]);
        }
        self.data = data;

        Some(Note { header, name, desc })
    }
}

#[inline]
fn align(offset: usize, size: usize) -> usize {
    (offset + (size - 1)) & !(size - 1)
}

/// A parsed `NoteHeader`.
#[derive(Debug)]
pub struct Note<'data, Elf>
where
    Elf: read::elf::FileHeader,
{
    header: &'data Elf::NoteHeader,
    name: &'data [u8],
    desc: &'data [u8],
}

impl<'data, Elf: read::elf::FileHeader> Note<'data, Elf> {
    /// Return the `n_type` field of the `NoteHeader`.
    ///
    /// The meaning of this field is determined by `name`.
    pub fn n_type(&self, endian: Elf::Endian) -> u32 {
        self.header.n_type(endian)
    }

    /// Return the bytes for the name field following the `NoteHeader`,
    /// excluding any null terminator.
    ///
    /// This field is usually a string including a null terminator
    /// (but it is not required to be).
    ///
    /// The length of this field (including any null terminator) is given by
    /// `n_namesz`.
    pub fn name(&self) -> &'data [u8] {
        if let Some((last, name)) = self.name.split_last() {
            if *last == 0 {
                return name;
            }
        }
        self.name
    }

    /// Return the bytes for the desc field following the `NoteHeader`.
    ///
    /// The length of this field is given by `n_descsz`. The meaning
    /// of this field is determined by `name` and `n_type`.
    pub fn desc(&self) -> &'data [u8] {
        self.desc
    }
}

#[cfg(test)]
mod test {
    use debugid::CodeId;

    use crate::linux_shared::kernel_symbols::parse_kallsyms;

    use super::build_id_from_notes_section_data;

    #[test]
    fn test() {
        let build_id = build_id_from_notes_section_data(b"\x04\0\0\0\x14\0\0\0\x03\0\0\0GNU\0\x98Kvo\x1c\xb5i\x9c;\x1bw\xb5\x92\x98<\"\xe9\xd1\x97\xad\x06\0\0\0\x04\0\0\0\x01\x01\0\0Linux\0\0\0\0\0\0\0\x06\0\0\0\x01\0\0\0\0\x01\0\0Linux\0\0\0\0\0\0\0");
        let code_id = CodeId::from_binary(build_id.unwrap());
        assert_eq!(code_id.as_str(), "984b766f1cb5699c3b1b77b592983c22e9d197ad");
    }

    #[test]
    fn test2() {
        let kallsyms = br#"ffff8000081e0000 T _text
ffff8000081f0000 t bcm2835_handle_irq
ffff8000081f0000 T _stext
ffff8000081f0000 T __irqentry_text_start
ffff8000081f0060 t bcm2836_arm_irqchip_handle_irq
ffff8000081f00e0 t dw_apb_ictl_handle_irq
ffff8000081f0190 t sun4i_handle_irq"#;
        let (base_avma, symbol_table) = parse_kallsyms(kallsyms).unwrap();
        assert_eq!(base_avma, 0xffff8000081e0000);
        assert_eq!(
            &symbol_table.lookup(0x10061).unwrap().name,
            "bcm2836_arm_irqchip_handle_irq"
        );
        assert_eq!(
            &symbol_table.lookup(0x10054).unwrap().name,
            "__irqentry_text_start"
        );
    }

    #[test]
    fn test3() {
        let kallsyms = br#"0000000000000000 A fixed_percpu_data
0000000000000000 A __per_cpu_start
0000000000001000 A cpu_debug_store
0000000000002000 A irq_stack_backing_store
0000000000006000 A cpu_tss_rw
0000000000032080 A steal_time
00000000000320c0 A apf_reason
0000000000033000 A __per_cpu_end
ffffffffa7e00000 T startup_64
ffffffffa7e00000 T _stext
ffffffffa7e00000 T _text
ffffffffa7e00040 T secondary_startup_64
ffffffffa7e00045 T secondary_startup_64_no_verify
ffffffffa7e00110 t verify_cpu
ffffffffa7e00210 T sev_verify_cbit"#;
        let (base_avma, symbol_table) = parse_kallsyms(kallsyms).unwrap();
        assert_eq!(base_avma, 0xffffffffa7e00000);
        assert_eq!(
            &symbol_table.lookup(0x61).unwrap().name,
            "secondary_startup_64_no_verify"
        );
    }
}
