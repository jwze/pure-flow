// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Bounded TDH schema cache. No guessed MOF offsets or EventHeader PID fallback.
use super::AlignedBuffer;
use crate::windows_schema;
use std::{
    collections::HashMap,
    mem::{offset_of, size_of},
    ptr::addr_of,
};
use windows::{
    core::GUID,
    Win32::{Foundation::ERROR_INSUFFICIENT_BUFFER, System::Diagnostics::Etw::*},
};
const MAX_SCHEMAS: usize = 128;
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key(GUID, u16, u8, u8, u16, u16);
struct Field {
    name: Vec<u16>,
    ty: u16,
}
struct Schema {
    fields: HashMap<String, Field>,
}
#[derive(Default)]
pub(super) struct Decoder {
    schemas: HashMap<Key, Schema>,
}
impl Decoder {
    fn schema(&mut self, record: &EVENT_RECORD) -> Result<&Schema, String> {
        let d = record.EventHeader.EventDescriptor;
        let key = Key(
            record.EventHeader.ProviderId,
            d.Id,
            d.Version,
            d.Opcode,
            d.Task,
            record.EventHeader.Flags
                & (EVENT_HEADER_FLAG_32_BIT_HEADER | EVENT_HEADER_FLAG_64_BIT_HEADER) as u16,
        );
        if !self.schemas.contains_key(&key) {
            if self.schemas.len() >= MAX_SCHEMAS {
                return Err("TDH schema cache limit exceeded".into());
            }
            let mut needed = 0;
            let rc = unsafe { TdhGetEventInformation(record, None, None, &mut needed) };
            if rc != ERROR_INSUFFICIENT_BUFFER.0 {
                return Err(format!("TDH schema size lookup failed: {rc}"));
            }
            if needed as usize > 1024 * 1024 || (needed as usize) < size_of::<TRACE_EVENT_INFO>() {
                return Err("TDH schema size invalid".into());
            }
            let mut buffer = AlignedBuffer::new(needed as usize);
            let info = buffer.ptr::<TRACE_EVENT_INFO>();
            let allocated = needed;
            let rc = unsafe { TdhGetEventInformation(record, None, Some(info), &mut needed) };
            if rc != 0 || needed > allocated {
                return Err(format!("TDH schema lookup failed: {rc}"));
            }
            let header = unsafe { &*info };
            let offset = offset_of!(TRACE_EVENT_INFO, EventPropertyInfoArray);
            let count = header.TopLevelPropertyCount as usize;
            if count > header.PropertyCount as usize
                || count > (buffer.len - offset) / size_of::<EVENT_PROPERTY_INFO>()
            {
                return Err("TDH property array outside metadata buffer".into());
            }
            let properties = unsafe {
                std::slice::from_raw_parts(
                    addr_of!((*info).EventPropertyInfoArray).cast::<EVENT_PROPERTY_INFO>(),
                    count,
                )
            };
            let mut fields = HashMap::new();
            for property in properties {
                let name = windows_schema::utf16_name(buffer.bytes(), property.NameOffset as usize)
                    .ok_or("TDH invalid property name")?;
                if !["pid", "processid", "size", "imagefilename"]
                    .contains(&name.to_ascii_lowercase().as_str())
                {
                    continue;
                }
                if property.Flags.0 != 0 || unsafe { property.Anonymous2.count } != 1 {
                    continue;
                }
                fields.insert(
                    name.to_ascii_lowercase(),
                    Field {
                        name: name.encode_utf16().chain([0]).collect(),
                        ty: unsafe { property.Anonymous1.nonStructType.InType },
                    },
                );
            }
            self.schemas.insert(key, Schema { fields });
        }
        self.schemas
            .get(&key)
            .ok_or_else(|| "TDH cache unavailable".into())
    }
    pub fn network(&mut self, record: &EVENT_RECORD) -> Result<(u32, u64), String> {
        let schema = self.schema(record)?;
        let pid = schema.pid(record)?;
        let size = schema
            .fields
            .get("size")
            .ok_or("ETW schema missing size")?
            .scalar(record)?;
        Ok((pid, size))
    }
    pub fn process(
        &mut self,
        record: &EVENT_RECORD,
        start: bool,
    ) -> Result<(u32, Option<String>), String> {
        let schema = self.schema(record)?;
        let pid = schema.pid(record)?;
        let name = if start {
            Some(
                schema
                    .fields
                    .get("imagefilename")
                    .ok_or("ETW process start schema missing ImageFileName")?
                    .text(record)?,
            )
        } else {
            None
        };
        Ok((pid, name))
    }
}
impl Schema {
    fn pid(&self, record: &EVENT_RECORD) -> Result<u32, String> {
        let value = self
            .fields
            .get("pid")
            .or_else(|| self.fields.get("processid"))
            .ok_or("ETW schema missing payload PID")?
            .scalar(record)?;
        if value == 0 || value > u32::MAX as u64 {
            return Err("ETW invalid payload PID".into());
        }
        Ok(value as u32)
    }
}
impl Field {
    fn descriptor(&self) -> PROPERTY_DATA_DESCRIPTOR {
        PROPERTY_DATA_DESCRIPTOR {
            PropertyName: self.name.as_ptr() as u64,
            ArrayIndex: u32::MAX,
            Reserved: 0,
        }
    }
    fn scalar(&self, record: &EVENT_RECORD) -> Result<u64, String> {
        let size = if self.ty == TDH_INTYPE_UINT32.0 as u16 {
            4
        } else if self.ty == TDH_INTYPE_UINT64.0 as u16 {
            8
        } else {
            return Err("ETW scalar has unsupported type".into());
        };
        let mut bytes = [0; 8];
        let rc = unsafe { TdhGetProperty(record, None, &[self.descriptor()], &mut bytes[..size]) };
        if rc != 0 {
            return Err(format!("TDH scalar read failed: {rc}"));
        }
        windows_schema::scalar(&bytes[..size], size)
    }
    fn text(&self, record: &EVENT_RECORD) -> Result<String, String> {
        if self.ty != TDH_INTYPE_ANSISTRING.0 as u16 && self.ty != TDH_INTYPE_UNICODESTRING.0 as u16
        {
            return Err("ETW image name has unsupported type".into());
        }
        let mut size = 0;
        let rc = unsafe { TdhGetPropertySize(record, None, &[self.descriptor()], &mut size) };
        if rc != 0 || size == 0 || size > 32768 {
            return Err(format!("TDH image name size invalid: {rc}"));
        }
        let mut bytes = vec![0; size as usize];
        let rc = unsafe { TdhGetProperty(record, None, &[self.descriptor()], &mut bytes) };
        if rc != 0 {
            return Err(format!("TDH image name read failed: {rc}"));
        }
        let text = if self.ty == TDH_INTYPE_UNICODESTRING.0 as u16 {
            windows_schema::utf16_name(&bytes, 0).ok_or("ETW invalid unicode image name")?
        } else {
            let end = bytes
                .iter()
                .position(|v| *v == 0)
                .ok_or("ETW image name missing terminator")?;
            // Reject non-UTF8 rather than invent a display name from PID or lossy aliases.
            std::str::from_utf8(&bytes[..end])
                .map_err(|_| "ETW image name encoding unsupported")?
                .to_owned()
        };
        text.rsplit(['\\', '/'])
            .next()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "ETW image name empty".into())
    }
}
