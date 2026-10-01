//! 控制端进程私有的目标选择；它独立于会重建的 Agent 和服务端会话。
//!
//! 只有首次在线列表和用户 local_use 可以改变选择。操作拿到目标 ID 副本后，即使另一个调用切换选择，
//! 上传预处理、网络等待和重连也不会把已开始的操作转给别的设备。

use ctrl_common::cmd_resp_info::{KikInfoVo, LocalNow};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
#[error("尚未选择 Kik，请先使用 $local_use <kik_id> 或提供 target_kik_id")]
pub struct NoTargetSelected;

#[derive(Default)]
struct Selection {
    /// 成功查询过首次列表后，即使列表为空，也不再自动选择后续上线的设备。
    initialized: bool,
    selected: Option<KikInfoVo>,
}

#[derive(Clone, Default)]
pub(crate) struct LocalTarget(Arc<Mutex<Selection>>);

impl LocalTarget {
    pub fn is_initialized(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .initialized
    }

    pub fn initialize(&self, online: Vec<KikInfoVo>) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.initialized {
            return;
        }
        state.initialized = true;
        if state.selected.is_none() {
            state.selected = online.into_iter().max_by(|left, right| {
                left.recent_online_time
                    .cmp(&right.recent_online_time)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
    }

    pub fn select(&self, selected: KikInfoVo) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        // 手动选择同样封住自动初始化入口，避免并发的首次查询晚返回后覆盖用户决定。
        state.initialized = true;
        state.selected = Some(selected);
    }

    pub fn snapshot(&self, explicit: Option<&str>) -> anyhow::Result<String> {
        let id = match explicit {
            Some(id) => id.to_owned(),
            None => self
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .selected
                .as_ref()
                .map(|kik| kik.id.clone())
                .ok_or(NoTargetSelected)?,
        };
        validate_target_id(&id)?;
        Ok(id)
    }

    /// 返回选择时的快照；没有在线状态推送，不能把这个结果解释为实时存活检查。
    pub fn current(&self) -> LocalNow {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .selected
            .clone()
            .map(LocalNow::Kik)
            .unwrap_or(LocalNow::None)
    }
}

pub(crate) fn validate_target_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !id.trim().is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
        "Kik ID 必须为 1..=128 bytes、非纯空白且不包含控制字符"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn kik(id: &str, seconds: u64) -> KikInfoVo {
        KikInfoVo {
            id: id.into(),
            name: id.into(),
            ip: "127.0.0.1".into(),
            recent_online_time: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
        }
    }

    #[test]
    fn first_selection_is_latest_and_does_not_follow_later_lists() {
        let targets = LocalTarget::default();
        targets.initialize(vec![kik("older", 1), kik("latest", 2)]);
        assert_eq!(targets.snapshot(None).unwrap(), "latest");
        targets.initialize(vec![kik("replacement", 3)]);
        assert_eq!(targets.snapshot(None).unwrap(), "latest");
        assert!(matches!(targets.current(), LocalNow::Kik(kik) if kik.id == "latest"));
        let empty = LocalTarget::default();
        empty.initialize(vec![]);
        empty.initialize(vec![kik("later", 4)]);
        assert!(empty.snapshot(None).unwrap_err().is::<NoTargetSelected>());
    }

    #[test]
    fn selection_is_shared_but_operation_snapshot_and_explicit_target_are_fixed() {
        let targets = LocalTarget::default();
        targets.select(kik("A", 1));
        let operation = targets.snapshot(None).unwrap();
        targets.clone().select(kik("B", 2));
        targets.initialize(vec![kik("C", 3)]);
        assert_eq!(operation, "A");
        assert_eq!(targets.snapshot(None).unwrap(), "B");
        assert_eq!(targets.snapshot(Some("explicit")).unwrap(), "explicit");
        assert_eq!(targets.snapshot(None).unwrap(), "B");
        for bad in [
            "".to_owned(),
            "   ".into(),
            "a\0b".into(),
            "a\nb".into(),
            "a\tb".into(),
            "a\u{0085}b".into(),
            "a".repeat(129),
        ] {
            assert!(targets.snapshot(Some(&bad)).is_err());
        }
    }
}
