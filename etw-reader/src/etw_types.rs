use std::ops::Deref;

use windows::Win32::System::Diagnostics::Etw;
use crate::{tdh_types::PropertyMapInfo};
use crate::schema::EventSchema;
use crate::utils;
use crate::tdh_types::Property;
use windows::core::GUID;
use windows::Win32::Foundation::PWSTR;

#[repr(transparent)]
pub struct EventRecord(Etw::EVENT_RECORD);

impl Deref for EventRecord {
    type Target = Etw::EVENT_RECORD;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl EventRecord {
    pub(crate) fn user_buffer(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.UserData as *mut _,
                self.UserDataLength.into(),
            )
        }
    }
}

/// Newtype wrapper over an [EVENT_PROPERTY_INFO]
///
/// [EVENT_PROPERTY_INFO]: https://microsoft.github.io/windows-docs-rs/doc/bindings/Windows/Win32/Etw/struct.EVENT_PROPERTY_INFO.html
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventPropertyInfo(Etw::EVENT_PROPERTY_INFO);

impl std::ops::Deref for EventPropertyInfo {
    type Target = Etw::EVENT_PROPERTY_INFO;

    fn deref(&self) -> &self::Etw::EVENT_PROPERTY_INFO {
        &self.0
    }
}

impl std::ops::DerefMut for EventPropertyInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<&[u8]> for EventPropertyInfo {
    fn from(val: &[u8]) -> Self {
        unsafe { *(val.as_ptr() as *mut EventPropertyInfo) }
    }
}

impl Default for EventPropertyInfo {
    fn default() -> Self {
        unsafe { std::mem::zeroed::<EventPropertyInfo>() }
    }
}

// Safe cast (EVENT_HEADER_FLAG_32_BIT_HEADER = 32)
#[doc(hidden)]
pub const EVENT_HEADER_FLAG_32_BIT_HEADER: u16 = Etw::EVENT_HEADER_FLAG_32_BIT_HEADER as u16;

/// Wrapper over the [DECODING_SOURCE] type
///
/// [DECODING_SOURCE]: https://microsoft.github.io/windows-docs-rs/doc/bindings/Windows/Win32/Etw/struct.DECODING_SOURCE.html
pub enum DecodingSource {
    DecodingSourceXMLFile,
    DecodingSourceWbem,
    DecodingSourceWPP,
    DecodingSourceTlg,
    DecodingSourceMax,
}

impl From<Etw::DECODING_SOURCE> for DecodingSource {
    fn from(val: Etw::DECODING_SOURCE) -> Self {
        match val {
            Etw::DecodingSourceXMLFile => DecodingSource::DecodingSourceXMLFile,
            Etw::DecodingSourceWbem => DecodingSource::DecodingSourceWbem,
            Etw::DecodingSourceWPP => DecodingSource::DecodingSourceWPP,
            Etw::DecodingSourceTlg => DecodingSource::DecodingSourceTlg,
            _ => DecodingSource::DecodingSourceMax,
        }
    }
}


/// Newtype wrapper over an [TRACE_EVENT_INFO]
///
/// [TRACE_EVENT_INFO]: https://microsoft.github.io/windows-docs-rs/doc/bindings/Windows/Win32/Etw/struct.TRACE_EVENT_INFO.html
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TraceEventInfo(Etw::TRACE_EVENT_INFO);

impl std::ops::Deref for TraceEventInfo {
    type Target = Etw::TRACE_EVENT_INFO;

    fn deref(&self) -> &self::Etw::TRACE_EVENT_INFO {
        &self.0
    }
}


#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct TraceEventInfoRaw {
    info: Vec<u8>,
}

impl std::ops::DerefMut for TraceEventInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<&TraceEventInfoRaw> for TraceEventInfo {
    fn from(val: &TraceEventInfoRaw) -> Self {
        unsafe { *(val.info.as_ptr() as *mut TraceEventInfo) }
    }
}


impl TraceEventInfoRaw {
    pub(crate) fn alloc(len: u32) -> Self {
        TraceEventInfoRaw {
            info: vec![0; len as usize],
        }
    }

    pub fn info_as_ptr(&mut self) -> *mut u8 {
        self.info.as_mut_ptr()
    }

    fn property_map_info(&self, index: u32) -> Option<PropertyMapInfo> {

        // let's make sure index is not bigger thant the PropertyCount
        assert!(index <= self.property_count());

        // We need to subtract the sizeof(EVENT_PROPERTY_INFO) due to how TRACE_EVENT_INFO is declared
        // in the bindings, the last field `EventPropertyInfoArray[ANYSIZE_ARRAY]` is declared as
        // [EVENT_PROPERTY_INFO; 1]
        // https://microsoft.github.io/windows-docs-rs/doc/bindings/Windows/Win32/Etw/struct.TRACE_EVENT_INFO.html#structfield.EventPropertyInfoArray
        let curr_prop_offset = index as usize * std::mem::size_of::<EventPropertyInfo>()
            + (std::mem::size_of::<TraceEventInfo>() - std::mem::size_of::<EventPropertyInfo>());

        let curr_prop = EventPropertyInfo::from(&self.info[curr_prop_offset..]);
        unsafe {
            if curr_prop.Anonymous1.nonStructType.MapNameOffset != 0 {
                let mut event: Etw::EVENT_RECORD = std::mem::zeroed();
                event.EventHeader.ProviderId = self.provider_guid();
                let mut buffer_size = 0;

                let map_name = PWSTR(self.info[curr_prop.Anonymous1.nonStructType.MapNameOffset as usize..].as_ptr() as *mut u16);
                use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
                // println!("map_name {}", utils::parse_unk_size_null_utf16_string(&self.info[curr_prop.Anonymous1.nonStructType.MapNameOffset as usize..]));

                if Etw::TdhGetEventMapInformation(&event, map_name, std::ptr::null_mut(), &mut buffer_size) != ERROR_INSUFFICIENT_BUFFER.0 {
                    panic!("expected this to fail");
                }
                
                let mut buffer = vec![0; buffer_size as usize];
                if Etw::TdhGetEventMapInformation(&event, map_name, buffer.as_mut_ptr() as *mut _, &mut buffer_size) != 0 {
                    panic!();
                }

                let map_info: &crate::Etw::EVENT_MAP_INFO = &*(buffer.as_ptr() as *const _);
                if map_info.Flag == crate::Etw::EVENTMAP_INFO_FLAG_MANIFEST_VALUEMAP || map_info.Flag == crate::Etw::EVENTMAP_INFO_FLAG_MANIFEST_BITMAP {
                    let is_bitmap = map_info.Flag == crate::Etw::EVENTMAP_INFO_FLAG_MANIFEST_BITMAP;
                    let mut map = crate::FastHashMap::default();
                    assert!(map_info.Anonymous.MapEntryValueType == crate::Etw::EVENTMAP_ENTRY_VALUETYPE_ULONG);
                    let entries = std::slice::from_raw_parts(map_info.MapEntryArray.as_ptr(), map_info.EntryCount as usize);
                    for e in entries {
                        let value = e.Anonymous.Value;
                        let name = utils::parse_unk_size_null_utf16_string(&buffer[e.OutputOffset as usize..]);
                        // println!("{} -> {:?}", value, name);
                        map.insert(value, name);
                    }
                    return Some(PropertyMapInfo { is_bitmap, map });
                } else  {
                    eprint!("unsupported map type {:?}", map_info.Flag);
                }

            }
        }
        return None;
    }
 
}

impl EventSchema for TraceEventInfoRaw {
    fn provider_guid(&self) -> GUID {
        TraceEventInfo::from(self).ProviderGuid
    }

    fn event_id(&self) -> u16 {
        TraceEventInfo::from(self).EventDescriptor.Id
    }

    fn opcode(&self) -> u8 {
        TraceEventInfo::from(self).EventDescriptor.Opcode
    }

    fn event_version(&self) -> u8 {
        TraceEventInfo::from(self).EventDescriptor.Version
    }
    
    fn level(&self) -> u8 {
        TraceEventInfo::from(self).EventDescriptor.Level
    }

    fn decoding_source(&self) -> DecodingSource {
        DecodingSource::from(TraceEventInfo::from(self).DecodingSource)
    }

    fn provider_name(&self) -> String {
        let provider_name_offset = TraceEventInfo::from(self).ProviderNameOffset as usize;
        // TODO: Evaluate performance, but this sounds better than creating a whole Vec<u16> and getting the string from the offset/2
        utils::parse_unk_size_null_utf16_string(&self.info[provider_name_offset..])
    }

    fn task_name(&self) -> String {
        let task_name_offset = TraceEventInfo::from(self).TaskNameOffset as usize;
        utils::parse_unk_size_null_utf16_string(&self.info[task_name_offset..])
    }

    fn opcode_name(&self) -> String {
        let opcode_name_offset = TraceEventInfo::from(self).OpcodeNameOffset as usize;
        if opcode_name_offset == 0 {
            return String::from("");
        }
        utils::parse_unk_size_null_utf16_string(&self.info[opcode_name_offset..])
    }
    
    fn property_count(&self) -> u32 {
        TraceEventInfo::from(self).PropertyCount
    }
    
    fn property(&self, index: u32) -> Property {
        // let's make sure index is not bigger thant the PropertyCount
        assert!(index <= self.property_count());

        // We need to subtract the sizeof(EVENT_PROPERTY_INFO) due to how TRACE_EVENT_INFO is declared
        // in the bindings, the last field `EventPropertyInfoArray[ANYSIZE_ARRAY]` is declared as
        // [EVENT_PROPERTY_INFO; 1]
        // https://microsoft.github.io/windows-docs-rs/doc/bindings/Windows/Win32/Etw/struct.TRACE_EVENT_INFO.html#structfield.EventPropertyInfoArray
        let curr_prop_offset = index as usize * std::mem::size_of::<EventPropertyInfo>()
            + (std::mem::size_of::<TraceEventInfo>() - std::mem::size_of::<EventPropertyInfo>());

        let curr_prop = EventPropertyInfo::from(&self.info[curr_prop_offset..]);
        let name =
            utils::parse_unk_size_null_utf16_string(&self.info[curr_prop.NameOffset as usize..]);
        Property::new(name, &curr_prop, self.property_map_info(index))
    }


}