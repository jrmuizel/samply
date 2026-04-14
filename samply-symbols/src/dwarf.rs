use std::borrow::Cow;
use std::collections::HashMap;
use std::marker::PhantomData;

use addr2line::{fallible_iterator, gimli};
use elsa::sync::FrozenVec;
use fallible_iterator::FallibleIterator;
use gimli::{DwarfPackage, EndianSlice, Reader, RunTimeEndian, SectionId};
use object::read::ReadRef;
use object::{CompressionFormat, Object, ObjectSection, ObjectSymbol};

use crate::{demangle, Error, FrameDebugInfo, SymbolMapStringInterner};

type RelocationMap = HashMap<usize, object::Relocation>;

fn add_relocations<'data, R: ReadRef<'data>>(
    relocations: &mut RelocationMap,
    file: &object::File<'data, R>,
    section: &object::Section<'data, '_, R>,
) {
    for (offset64, mut relocation) in section.relocations() {
        let offset = offset64 as usize;
        if offset as u64 != offset64 {
            continue;
        }
        match relocation.kind() {
            object::RelocationKind::Absolute => {
                match relocation.target() {
                    object::RelocationTarget::Symbol(symbol_idx) => {
                        match file.symbol_by_index(symbol_idx) {
                            Ok(symbol) => {
                                let addend =
                                    symbol.address().wrapping_add(relocation.addend() as u64);
                                relocation.set_addend(addend as i64);
                            }
                            Err(_) => {
                                eprintln!(
                                    "Relocation with invalid symbol for section {} at offset 0x{:08x}",
                                    section.name().unwrap_or("<unknown>"),
                                    offset
                                );
                            }
                        }
                    }
                    _ => {}
                }
                if relocations.insert(offset, relocation).is_some() {
                    eprintln!(
                        "Multiple relocations for section {} at offset 0x{:08x}",
                        section.name().unwrap_or("<unknown>"),
                        offset
                    );
                }
            }
            _ => {
                eprintln!(
                    "Unsupported relocation for section {} at offset 0x{:08x}",
                    section.name().unwrap_or("<unknown>"),
                    offset
                );
            }
        }
    }
}

/// Apply relocations to addresses and offsets during parsing.
/// This is necessary for relocatable object files (.o files) where
/// DWARF sections contain unrelocated addresses.
#[derive(Debug, Clone)]
pub struct Relocate<'a, R: gimli::Reader<Offset = usize>> {
    pub(crate) relocations: &'a RelocationMap,
    pub(crate) section: R,
    pub(crate) reader: R,
}

impl<'a, R: gimli::Reader<Offset = usize>> Relocate<'a, R> {
    fn relocate(&self, offset: usize, value: u64) -> u64 {
        if let Some(relocation) = self.relocations.get(&offset) {
            match relocation.kind() {
                object::RelocationKind::Absolute => {
                    if relocation.has_implicit_addend() {
                        return value.wrapping_add(relocation.addend() as u64);
                    } else {
                        return relocation.addend() as u64;
                    }
                }
                _ => {}
            }
        }
        value
    }
}

impl<'a, R: gimli::Reader<Offset = usize>> gimli::Reader for Relocate<'a, R> {
    type Endian = R::Endian;
    type Offset = R::Offset;

    fn read_address(&mut self, address_size: u8) -> gimli::Result<u64> {
        let offset = self.reader.offset_from(&self.section);
        let value = self.reader.read_address(address_size)?;
        Ok(self.relocate(offset, value))
    }

    fn read_length(&mut self, format: gimli::Format) -> gimli::Result<usize> {
        let offset = self.reader.offset_from(&self.section);
        let value = self.reader.read_length(format)?;
        <usize as gimli::ReaderOffset>::from_u64(self.relocate(offset, value as u64))
    }

    fn read_offset(&mut self, format: gimli::Format) -> gimli::Result<usize> {
        let offset = self.reader.offset_from(&self.section);
        let value = self.reader.read_offset(format)?;
        <usize as gimli::ReaderOffset>::from_u64(self.relocate(offset, value as u64))
    }

    fn read_sized_offset(&mut self, size: u8) -> gimli::Result<usize> {
        let offset = self.reader.offset_from(&self.section);
        let value = self.reader.read_sized_offset(size)?;
        <usize as gimli::ReaderOffset>::from_u64(self.relocate(offset, value as u64))
    }

    #[inline]
    fn split(&mut self, len: Self::Offset) -> gimli::Result<Self> {
        let mut other = self.clone();
        other.reader.truncate(len)?;
        self.reader.skip(len)?;
        Ok(other)
    }

    #[inline]
    fn endian(&self) -> Self::Endian {
        self.reader.endian()
    }

    #[inline]
    fn len(&self) -> Self::Offset {
        self.reader.len()
    }

    #[inline]
    fn empty(&mut self) {
        self.reader.empty()
    }

    #[inline]
    fn truncate(&mut self, len: Self::Offset) -> gimli::Result<()> {
        self.reader.truncate(len)
    }

    #[inline]
    fn offset_from(&self, base: &Self) -> Self::Offset {
        self.reader.offset_from(&base.reader)
    }

    #[inline]
    fn offset_id(&self) -> gimli::ReaderOffsetId {
        self.reader.offset_id()
    }

    #[inline]
    fn lookup_offset_id(&self, id: gimli::ReaderOffsetId) -> Option<Self::Offset> {
        self.reader.lookup_offset_id(id)
    }

    #[inline]
    fn find(&self, byte: u8) -> gimli::Result<Self::Offset> {
        self.reader.find(byte)
    }

    #[inline]
    fn skip(&mut self, len: Self::Offset) -> gimli::Result<()> {
        self.reader.skip(len)
    }

    #[inline]
    fn to_slice(&self) -> gimli::Result<Cow<'_, [u8]>> {
        self.reader.to_slice()
    }

    #[inline]
    fn to_string(&self) -> gimli::Result<Cow<'_, str>> {
        self.reader.to_string()
    }

    #[inline]
    fn to_string_lossy(&self) -> gimli::Result<Cow<'_, str>> {
        self.reader.to_string_lossy()
    }

    #[inline]
    fn read_slice(&mut self, buf: &mut [u8]) -> gimli::Result<()> {
        self.reader.read_slice(buf)
    }
}

pub fn get_frames<R: Reader>(
    address: u64,
    context: Option<&addr2line::Context<R>>,
    string_interner: &mut SymbolMapStringInterner,
) -> Option<Vec<FrameDebugInfo>> {
    let frame_iter = context?.find_frames(address).skip_all_loads().ok()?;
    convert_frames(frame_iter, string_interner)
}

pub fn convert_frames<'a, R: gimli::Reader>(
    frame_iter: impl FallibleIterator<Item = addr2line::Frame<'a, R>>,
    string_interner: &mut SymbolMapStringInterner,
) -> Option<Vec<FrameDebugInfo>> {
    let frames: Vec<_> = frame_iter
        .map(|f| Ok(convert_stack_frame(f, string_interner)))
        .collect()
        .ok()?;

    if frames.is_empty() {
        None
    } else {
        Some(frames)
    }
}

pub fn convert_stack_frame<R: gimli::Reader>(
    frame: addr2line::Frame<R>,
    string_interner: &mut SymbolMapStringInterner,
) -> FrameDebugInfo {
    let function = match frame.function {
        Some(function_name) => {
            if let Ok(name) = function_name.raw_name() {
                let name = demangle::demangle_any(&name);
                Some(string_interner.intern_owned(&name).into())
            } else {
                None
            }
        }
        None => None,
    };
    let file_path = frame
        .location
        .as_ref()
        .and_then(|l| l.file)
        .map(|file| string_interner.intern_owned(file).into());

    FrameDebugInfo {
        function,
        file_path,
        line_number: frame.location.and_then(|l| l.line),
        ..Default::default()
    }
}

pub enum SingleSectionData<'data, T: ReadRef<'data>> {
    View {
        data: T,
        offset: u64,
        size: u64,
        _phantom: PhantomData<&'data ()>,
    },
    Owned(Vec<u8>),
}

pub fn try_get_section_data<'data, O, T>(
    data: T,
    file: &O,
    section_id: SectionId,
    is_for_dwo_dwp: bool,
) -> Option<SingleSectionData<'data, T>>
where
    O: object::Object<'data>,
    T: ReadRef<'data>,
{
    use object::ObjectSection;
    let section_name = if is_for_dwo_dwp {
        section_id.dwo_name()?
    } else {
        section_id.name()
    };
    let section = file.section_by_name(section_name)?;
    let file_range = section.compressed_file_range().ok()?;
    match file_range.format {
        CompressionFormat::None => Some(SingleSectionData::View {
            data,
            offset: file_range.offset,
            size: file_range.uncompressed_size,
            _phantom: PhantomData,
        }),
        _ => {
            let compressed = file_range.data(data).ok()?;
            let decompressed = compressed.decompress().ok()?;
            Some(SingleSectionData::Owned(decompressed.into_owned()))
        }
    }
}

/// Holds on to section data so that we can create an addr2line::Context for that
/// that data. This avoids one copy compared to what addr2line::Context::new does
/// by default, saving 1.5 seconds on libxul. (For comparison, dumping all symbols
/// from libxul takes 200ms in total.)
/// See addr2line::Context::new for details.
pub struct Addr2lineContextData {
    uncompressed_section_data: FrozenVec<Vec<u8>>,
    relocations: FrozenVec<Box<RelocationMap>>,
}

impl Addr2lineContextData {
    pub fn new() -> Self {
        Self {
            uncompressed_section_data: FrozenVec::new(),
            relocations: FrozenVec::new(),
        }
    }

    fn sect<'data, 'ctxdata, O, R>(
        &'ctxdata self,
        data: R,
        obj: &O,
        section_id: SectionId,
        endian: RunTimeEndian,
        is_for_dwo_dwp: bool,
    ) -> EndianSlice<'ctxdata, RunTimeEndian>
    where
        'data: 'ctxdata,
        O: object::Object<'data>,
        R: ReadRef<'data>,
    {
        let slice: &[u8] = match try_get_section_data(data, obj, section_id, is_for_dwo_dwp) {
            Some(SingleSectionData::Owned(section_data)) => {
                self.uncompressed_section_data.push_get(section_data)
            }
            Some(SingleSectionData::View {
                data, offset, size, ..
            }) => data.read_bytes_at(offset, size).unwrap_or(&[]),
            None => &[],
        };
        EndianSlice::new(slice, endian)
    }

    pub fn make_context<'data, 'ctxdata, R>(
        &'ctxdata self,
        data: R,
        obj: &object::File<'data, R>,
        sup_data: Option<R>,
        sup_obj: Option<&object::File<'data, R>>,
    ) -> Result<addr2line::Context<Relocate<'ctxdata, EndianSlice<'ctxdata, RunTimeEndian>>>, Error>
    where
        'data: 'ctxdata,
        R: ReadRef<'data>,
    {
        let e = if obj.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        // Always collect and apply relocations. For non-relocatable files,
        // the relocation maps will be empty and have no effect.
        let mut dwarf = gimli::Dwarf::load(|section_id| {
            self.load_section_with_relocations(data, obj, section_id, e, false)
        })
        .map_err(Error::Addr2lineContextCreationError)?;

        if let (Some(sup_obj), Some(sup_data)) = (sup_obj, sup_data) {
            dwarf
                .load_sup(|section_id| {
                    self.load_section_with_relocations(sup_data, sup_obj, section_id, e, false)
                })
                .map_err(Error::Addr2lineContextCreationError)?;
        }

        let context =
            addr2line::Context::from_dwarf(dwarf).map_err(Error::Addr2lineContextCreationError)?;
        Ok(context)
    }

    fn load_section_with_relocations<'data, 'ctxdata, R>(
        &'ctxdata self,
        data: R,
        obj: &object::File<'data, R>,
        section_id: SectionId,
        endian: RunTimeEndian,
        is_for_dwo_dwp: bool,
    ) -> Result<Relocate<'ctxdata, EndianSlice<'ctxdata, RunTimeEndian>>, gimli::Error>
    where
        'data: 'ctxdata,
        R: ReadRef<'data>,
    {
        // Get the section data
        let slice = self.sect(data, obj, section_id, endian, is_for_dwo_dwp);

        // Collect relocations for this section
        let mut relocations = RelocationMap::default();
        let section_name = if is_for_dwo_dwp {
            section_id.dwo_name()
        } else {
            Some(section_id.name())
        };

        if let Some(name) = section_name {
            if let Some(section) = obj.section_by_name(name) {
                // DWO sections never have relocations
                if !is_for_dwo_dwp {
                    add_relocations(&mut relocations, obj, &section);
                }
            }
        }

        // Store the relocations and wrap the reader
        let relocations_ref = self.relocations.push_get(Box::new(relocations));

        Ok(Relocate {
            relocations: relocations_ref,
            section: slice,
            reader: slice,
        })
    }

    pub fn make_package<'data, 'ctxdata, R>(
        &'ctxdata self,
        data: R,
        obj: &object::File<'data, R>,
        dwp_data: Option<R>,
        dwp_obj: Option<&object::File<'data, R>>,
    ) -> Result<Option<DwarfPackage<Relocate<'ctxdata, EndianSlice<'ctxdata, RunTimeEndian>>>>, Error>
    where
        'data: 'ctxdata,
        R: ReadRef<'data>,
    {
        let e = if obj.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };
        let empty_relocs = self.relocations.push_get(Box::new(HashMap::new()));
        let mut package = None;
        if let (Some(dwp_obj), Some(dwp_data)) = (dwp_obj, dwp_data) {
            package = DwarfPackage::load::<_, gimli::Error>(
                |section_id| self.load_section_with_relocations(dwp_data, dwp_obj, section_id, e, true),
                Relocate {
                    relocations: empty_relocs,
                    section: EndianSlice::new(&[], e),
                    reader: EndianSlice::new(&[], e),
                },
            )
            .ok();
        }
        if package.is_none() && obj.section_by_name(".debug_cu_index").is_some() {
            package = DwarfPackage::load::<_, gimli::Error>(
                |section_id| self.load_section_with_relocations(data, obj, section_id, e, true),
                Relocate {
                    relocations: empty_relocs,
                    section: EndianSlice::new(&[], e),
                    reader: EndianSlice::new(&[], e),
                },
            )
            .ok();
        }
        Ok(package)
    }

    pub fn make_dwarf_for_dwo<'data, 'ctxdata, R>(
        &'ctxdata self,
        data: R,
        obj: &object::File<'data, R>,
    ) -> Result<addr2line::gimli::Dwarf<Relocate<'ctxdata, EndianSlice<'ctxdata, RunTimeEndian>>>, Error>
    where
        'data: 'ctxdata,
        R: ReadRef<'data>,
    {
        let e = if obj.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };
        // DWO files typically don't have relocations, but we use the same wrapper for consistency
        let dwarf = gimli::Dwarf::load(|section_id| {
            self.load_section_with_relocations(data, obj, section_id, e, true)
        })
        .map_err(Error::Addr2lineContextCreationError)?;
        Ok(dwarf)
    }
}
