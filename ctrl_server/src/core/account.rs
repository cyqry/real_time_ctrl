//! 多账号配置、Kik ACL 和账号级并发策略。
//!
//! 配置只在启动时解析为不可变 `AccountRegistry`。账号间不共享 secret 或命令许可；克隆策略只克隆
//! `Arc`，因此同账号的所有会话共同受到账号级信号量约束。

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub const DEFAULT_MAX_INSTANCES: usize = 32;
pub const DEFAULT_MAX_COMMANDS_PER_ACCOUNT: usize = 64;
pub const DEFAULT_MAX_COMMANDS_PER_INSTANCE: usize = 16;
const MAX_ACCOUNTS: usize = 256;
const MAX_ALLOWED_KIKS: usize = 1024;

#[derive(Clone)]
/// 启动后只读的账号索引。
pub struct AccountRegistry(Arc<HashMap<String, AccountPolicy>>);

#[derive(Clone)]
/// 一个账号的认证材料、ACL 和资源配额。
pub struct AccountPolicy {
    pub secret: Arc<str>,
    pub max_instances: usize,
    pub max_commands_per_instance: usize,
    pub command_limit: Arc<Semaphore>,
    allowed_kiks: Arc<HashSet<String>>,
    allow_all_kiks: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountInput {
    account_id: String,
    secret: String,
    #[serde(default)]
    allowed_kiks: Vec<String>,
    #[serde(default = "default_max_instances")]
    max_instances: usize,
    #[serde(default = "default_max_commands")]
    max_commands_per_instance: usize,
    #[serde(default = "default_max_account_commands")]
    max_commands_per_account: usize,
}

impl AccountRegistry {
    /// JSON 配置用于多账号；未配置时由旧的单秘密变量生成 `default` 账号，
    /// 让现有单文件部署无需额外文件即可直接启动。
    pub fn from_json_or_default(
        json: Option<&str>,
        default_secret: String,
    ) -> anyhow::Result<Self> {
        let inputs = match json {
            Some(json) if !json.trim().is_empty() => {
                serde_json::from_str::<Vec<AccountInput>>(json)?
            }
            _ => vec![AccountInput {
                account_id: "default".to_string(),
                secret: default_secret,
                allowed_kiks: vec!["*".to_string()],
                max_instances: DEFAULT_MAX_INSTANCES,
                max_commands_per_instance: DEFAULT_MAX_COMMANDS_PER_INSTANCE,
                max_commands_per_account: DEFAULT_MAX_COMMANDS_PER_ACCOUNT,
            }],
        };
        if inputs.is_empty() || inputs.len() > MAX_ACCOUNTS {
            anyhow::bail!("控制账号数量必须在 1..={MAX_ACCOUNTS} 范围内");
        }

        let mut accounts = HashMap::with_capacity(inputs.len());
        for input in inputs {
            validate_identity(&input.account_id)?;
            if input.secret.len() < 32 || !input.secret.is_ascii() {
                anyhow::bail!(
                    "账号 {} 的 secret 必须至少 32 个 ASCII 字符",
                    input.account_id
                );
            }
            if !(1..=128).contains(&input.max_instances)
                || !(1..=64).contains(&input.max_commands_per_instance)
                || !(1..=512).contains(&input.max_commands_per_account)
            {
                anyhow::bail!("账号 {} 的并发限制超出安全范围", input.account_id);
            }
            if input.allowed_kiks.len() > MAX_ALLOWED_KIKS {
                anyhow::bail!("账号 {} 的 Kik ACL 超出上限", input.account_id);
            }
            let allow_all_kiks = input.allowed_kiks.iter().any(|id| id == "*");
            let mut allowed_kiks = HashSet::new();
            for id in input.allowed_kiks.into_iter().filter(|id| id != "*") {
                uuid::Uuid::parse_str(&id)
                    .map_err(|_| anyhow::anyhow!("账号 {} 含无效 Kik UUID", input.account_id))?;
                allowed_kiks.insert(id);
            }
            let policy = AccountPolicy {
                secret: Arc::from(input.secret),
                max_instances: input.max_instances,
                max_commands_per_instance: input.max_commands_per_instance,
                command_limit: Arc::new(Semaphore::new(input.max_commands_per_account)),
                allowed_kiks: Arc::new(allowed_kiks),
                allow_all_kiks,
            };
            if accounts.insert(input.account_id.clone(), policy).is_some() {
                anyhow::bail!("控制账号 ID 重复: {}", input.account_id);
            }
        }
        Ok(Self(Arc::new(accounts)))
    }

    pub fn get(&self, account_id: &str) -> Option<AccountPolicy> {
        self.0.get(account_id).cloned()
    }
}

impl AccountPolicy {
    pub fn allows_kik(&self, kik_id: &str) -> bool {
        self.allow_all_kiks || self.allowed_kiks.contains(kik_id)
    }
}

fn validate_identity(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("账号 ID 必须是 1..64 位 ASCII 字母、数字、- 或 _");
    }
    Ok(())
}

const fn default_max_instances() -> usize {
    DEFAULT_MAX_INSTANCES
}

const fn default_max_commands() -> usize {
    DEFAULT_MAX_COMMANDS_PER_INSTANCE
}

const fn default_max_account_commands() -> usize {
    DEFAULT_MAX_COMMANDS_PER_ACCOUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_acl_isolated_from_other_kiks() {
        let kik = uuid::Uuid::new_v4().to_string();
        let json = format!(
            r#"[{{"account_id":"tenant_a","secret":"01234567890123456789012345678901","allowed_kiks":["{kik}"]}}]"#
        );
        let registry = AccountRegistry::from_json_or_default(Some(&json), String::new()).unwrap();
        let policy = registry.get("tenant_a").unwrap();
        assert!(policy.allows_kik(&kik));
        assert!(!policy.allows_kik(&uuid::Uuid::new_v4().to_string()));
    }

    #[test]
    fn hostile_account_configuration_is_rejected_fail_closed() {
        let cases = [
            // 未知字段不能被静默忽略，否则运维拼错安全配置时会以意外默认值启动。
            r#"[{"account_id":"a","secret":"01234567890123456789012345678901","unknown":true}]"#,
            r#"[{"account_id":"a","secret":"too-short"}]"#,
            r#"[{"account_id":"../a","secret":"01234567890123456789012345678901"}]"#,
            r#"[{"account_id":"a","secret":"01234567890123456789012345678901","max_instances":0}]"#,
            r#"[{"account_id":"a","secret":"01234567890123456789012345678901","max_commands_per_instance":65}]"#,
            r#"[{"account_id":"a","secret":"01234567890123456789012345678901","allowed_kiks":["not-a-uuid"]}]"#,
            r#"[
                {"account_id":"a","secret":"01234567890123456789012345678901"},
                {"account_id":"a","secret":"abcdefghijabcdefghijabcdefghijab"}
            ]"#,
        ];

        for json in cases {
            assert!(
                AccountRegistry::from_json_or_default(Some(json), String::new()).is_err(),
                "不安全的账号配置被接受: {json}"
            );
        }
    }
}
