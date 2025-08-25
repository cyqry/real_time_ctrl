use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};
use std::fmt::{Debug, Formatter};
use std::process::id;
#[derive(Clone, Debug)]
pub struct KikInfo {
    pub id: Option<String>,
    pub name: String,
}

impl BufSerializable for KikInfo {
    fn to_buf(&self) -> BytesMut {
        let mut bytes_mut = BytesMut::new();
        match &self.id {
            None => {
                bytes_mut.put_u8(0);
                bytes_mut.put_slice(self.name.as_bytes());
                bytes_mut
            }
            Some(id) => {
                bytes_mut.put_u8(1);
                bytes_mut.put_u32(id.as_bytes().len() as u32);
                bytes_mut.put_slice(id.as_bytes());
                bytes_mut.put_slice(self.name.as_bytes());
                bytes_mut
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self>
    where
        Self: Sized,
    {
        let code = bys.get_u8();
        match code {
            0 => Some(KikInfo {
                id: None,
                name: String::from_utf8(bys.to_vec()).ok()?,
            }),
            1 => {
                let id_len = bys.get_u32();
                Some(KikInfo {
                    id: Some(String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?),
                    name: String::from_utf8(bys.to_vec()).ok()?,
                })
            }
            _ => None,
        }
    }
}
