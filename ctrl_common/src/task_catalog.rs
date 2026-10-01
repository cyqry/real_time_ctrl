//! 只读任务目录的管理面契约。只返回任务名，不携带服务器路径、参数或二进制内容。
use bytes::{Buf, BufMut, BytesMut};
use common::protocol::{BufSerializable, MAX_CORRELATION_ID_BYTES};
use serde::{Deserialize, Serialize};

pub const MAX_TASK_LIST_BYTES: usize = 512;
pub const MAX_TASK_LIST_LIMIT: u16 = 50;
pub const DEFAULT_TASK_LIST_LIMIT: u16 = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListRequest {
    pub id: String,
    pub target: String,
    pub query: String,
    pub cursor: Option<String>,
    pub limit: u16,
}

impl TaskListRequest {
    pub fn valid(&self) -> bool {
        identifier(&self.id, MAX_CORRELATION_ID_BYTES)
            && identifier(&self.target, crate::ctrl_frame::MAX_TARGET_ID_BYTES)
            && valid_query(&self.query)
            && self
                .cursor
                .as_deref()
                .is_none_or(common::task::valid_task_name)
            && (1..=MAX_TASK_LIST_LIMIT).contains(&self.limit)
    }
}

pub fn valid_query(query: &str) -> bool {
    query.is_empty() || common::task::valid_task_name(query)
}

fn identifier(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListPage {
    pub tasks: Vec<String>,
    /// 指向最后检查的目录名，空页面也可能有下一页，客户端不能用 tasks.is_empty 判断结束。
    pub next_cursor: Option<String>,
}

impl BufSerializable for TaskListRequest {
    fn to_buf(&self) -> BytesMut {
        // 非法内部构造也编码为不可解析的空正文，避免先为超长输入分配巨量内存。
        if !self.valid() {
            return BytesMut::new();
        }
        let mut bytes = BytesMut::new();
        for value in [&self.id, &self.target, &self.query] {
            put_text(&mut bytes, value);
        }
        match &self.cursor {
            Some(cursor) => {
                bytes.put_u8(1);
                put_text(&mut bytes, cursor);
            }
            None => bytes.put_u8(0),
        }
        bytes.put_u16(self.limit);
        bytes
    }

    fn from_buf(mut bytes: BytesMut) -> Option<Self> {
        if bytes.len() > MAX_TASK_LIST_BYTES {
            return None;
        }
        let id = take_text(&mut bytes, MAX_CORRELATION_ID_BYTES)?;
        let target = take_text(&mut bytes, crate::ctrl_frame::MAX_TARGET_ID_BYTES)?;
        let query = take_text(&mut bytes, 64)?;
        if bytes.is_empty() {
            return None;
        }
        let cursor = match bytes.get_u8() {
            0 => None,
            1 => Some(take_text(&mut bytes, 64)?),
            _ => return None,
        };
        if bytes.len() != 2 {
            return None;
        }
        let request = Self {
            id,
            target,
            query,
            cursor,
            limit: bytes.get_u16(),
        };
        request.valid().then_some(request)
    }
}

fn put_text(bytes: &mut BytesMut, text: &str) {
    bytes.put_u16(text.len() as u16);
    bytes.put_slice(text.as_bytes());
}

fn take_text(bytes: &mut BytesMut, maximum: usize) -> Option<String> {
    if bytes.len() < 2 {
        return None;
    }
    let length = bytes.get_u16() as usize;
    if length > maximum || bytes.len() < length {
        return None;
    }
    String::from_utf8(bytes.split_to(length).to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctrl_frame::Frame;

    fn request() -> TaskListRequest {
        TaskListRequest {
            id: "request-1".into(),
            target: "kik-a".into(),
            query: "TaSk_".into(),
            cursor: Some("task_01".into()),
            limit: 32,
        }
    }

    #[test]
    fn task_list_frame_round_trip_and_every_truncation_fails_closed() {
        let request = request();
        let encoded = Frame::TaskList(request.clone()).to_buf();
        assert_eq!(encoded[0], 20);
        assert!(
            matches!(Frame::from_buf(encoded.clone()), Some(Frame::TaskList(actual)) if actual == request)
        );
        for end in 0..encoded.len() {
            assert!(Frame::from_buf(encoded[..end].into()).is_none());
        }
        let mut trailing = encoded.clone();
        trailing.extend_from_slice(&[0]);
        assert!(Frame::from_buf(trailing).is_none());
        let mut unknown_cursor = request.to_buf();
        let marker = 6 + request.id.len() + request.target.len() + request.query.len();
        unknown_cursor[marker] = 2;
        assert!(TaskListRequest::from_buf(unknown_cursor).is_none());
    }

    #[test]
    fn task_list_rejects_invalid_fields_and_oversized_wire_bodies() {
        for query in ["../task", "任务", "x y", &"x".repeat(65)] {
            let mut value = request();
            value.query = query.into();
            assert!(!value.valid());
        }
        for limit in [0, 51, u16::MAX] {
            let mut value = request();
            value.limit = limit;
            assert!(!value.valid());
        }
        for id in ["", "\n", &"x".repeat(129)] {
            let mut value = request();
            value.id = id.into();
            assert!(!value.valid());
        }
        for target in ["", "\n", &"x".repeat(129)] {
            let mut value = request();
            value.target = target.into();
            assert!(!value.valid());
        }
        for cursor in ["", "../x", &"x".repeat(65)] {
            let mut value = request();
            value.cursor = Some(cursor.into());
            assert!(!value.valid());
        }
        assert!(TaskListRequest::from_buf(BytesMut::from(
            vec![0u8; MAX_TASK_LIST_BYTES + 1].as_slice()
        ))
        .is_none());
        let mut value = request();
        value.query.clear();
        value.cursor = None;
        assert_eq!(TaskListRequest::from_buf(value.to_buf()), Some(value));
    }
}
