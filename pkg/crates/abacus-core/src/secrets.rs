//! SecretsManager — 敏感数据安全管理
//!
//! ## 依赖
//! - `zeroize`: 安全清零内存
//! - `getrandom`: CSPRNG 生成密钥
//!
//! ## 引用关系
//! - 被 `CoreLoop` 初始化时加载 API 密钥
//! - 被 `DeepSeekProvider` 通过 `get_api_key()` 获取密钥
//! - 被 `McipGateway` 通过 `get_hmac_key()` 获取 HMAC 密钥
//!
//! ## 安全特性
//! - SecretString: Drop 时清零内存
//! - mlock: 防止密钥被 swap 到磁盘 (Linux/macOS)
//! - 审计日志: 每次访问记录时间戳

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 安全字符串，Drop 时清零内存
///
/// ## 安全保证
/// - `zeroize` crate 确保内存被覆盖为零
/// - `mlock` (可选) 防止被 swap 到磁盘
/// - Debug 格式化输出 `[REDACTED]`
pub struct SecretString {
    inner: Vec<u8>,
    #[allow(dead_code)]
    locked: bool,
}

impl SecretString {
    /// 从字符串创建 (mlock 在最终分配后执行)
    pub fn new(s: impl Into<Vec<u8>>) -> Self {
        let inner = s.into();
        // mlock after final allocation — prevents allocator from leaving old pages unlocked
        let locked = Self::mlock(&inner).is_ok();
        Self { inner, locked }
    }

    /// 从 CSPRNG 生成随机密钥 (pre-allocated buffer, mlock before fill)
    ///
    /// ## 错误处理（C6 修复）
    /// CSPRNG 失败（容器启动期 entropy 不足等罕见情况）返回 `Err`，
    /// 而非 panic。调用方决定是 fail-fast 还是 fallback，避免整进程崩溃。
    pub fn generate(length: usize) -> Result<Self, getrandom::Error> {
        // Pre-allocate exact size to avoid reallocation after mlock
        let mut key = vec![0u8; length];
        // mlock the pre-allocated buffer before filling with random data
        let locked = Self::mlock(&key).is_ok();
        getrandom::fill(&mut key)?;
        Ok(Self { inner: key, locked })
    }

    /// 获取密钥引用 (不复制)
    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    /// 获取密钥字符串 (如果有效 UTF-8)
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.inner).ok()
    }

    /// 尝试锁定内存页面 (防止 swap)
    #[cfg(unix)]
    fn mlock(data: &[u8]) -> std::io::Result<()> {
        use std::os::raw::{c_int, c_void};
        extern "C" {
            fn mlock(addr: *const c_void, len: usize) -> c_int;
        }
        let ret = unsafe { mlock(data.as_ptr() as *const c_void, data.len()) };
        if ret == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }

    #[cfg(not(unix))]
    fn mlock(_data: &[u8]) -> std::io::Result<()> {
        // mlock not available on this platform
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mlock not available"))
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.inner.zeroize();
        // Release mlock to avoid exhausting locked page quota
        if self.locked {
            Self::munlock(&self.inner);
        }
    }
}

impl SecretString {
    #[cfg(unix)]
    fn munlock(data: &[u8]) {
        use std::os::raw::{c_int, c_void};
        extern "C" {
            fn munlock(addr: *const c_void, len: usize) -> c_int;
        }
        unsafe { munlock(data.as_ptr() as *const c_void, data.len()); }
    }

    #[cfg(not(unix))]
    fn munlock(_data: &[u8]) {}
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// 密钥类型
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum SecretType {
    ApiKey(String),
    HmacKey,
    TlsCert,
    TlsKey,
    Custom(String),
}

/// 审计记录
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub secret_type: SecretType,
    pub accessed_at: chrono::DateTime<chrono::Utc>,
    pub accessed_by: String,
}

/// SecretsManager — 管理所有敏感数据
///
/// P3-B: audit_log 改用 BoundedFifo（替代 Vec.remove(0) O(n) 滚动）
const AUDIT_LOG_MAX: usize = 1000;

pub struct SecretsManager {
    secrets: RwLock<HashMap<SecretType, Arc<SecretString>>>,
    audit_log: RwLock<abacus_types::BoundedFifo<AuditRecord>>,
}

impl SecretsManager {
    /// 创建新的 SecretsManager
    pub fn new() -> Self {
        Self {
            secrets: RwLock::new(HashMap::new()),
            audit_log: RwLock::new(abacus_types::BoundedFifo::new(AUDIT_LOG_MAX)),
        }
    }

    /// 存储密钥
    pub async fn store(&self, type_: SecretType, secret: SecretString) {
        let mut secrets = self.secrets.write().await;
        secrets.insert(type_.clone(), Arc::new(secret));
    }

    /// 获取密钥 (记录审计日志，上限 AUDIT_LOG_MAX 条；P3-B: BoundedFifo 自动 evict)
    pub async fn get(&self, type_: &SecretType, accessed_by: &str) -> Option<Arc<SecretString>> {
        let secrets = self.secrets.read().await;
        let secret = secrets.get(type_).cloned();
        if secret.is_some() {
            self.audit_log.write().await.push(AuditRecord {
                secret_type: type_.clone(),
                accessed_at: chrono::Utc::now(),
                accessed_by: accessed_by.to_string(),
            });
        }
        secret
    }

    /// 删除密钥
    pub async fn remove(&self, type_: &SecretType) -> bool {
        let mut secrets = self.secrets.write().await;
        secrets.remove(type_).is_some()
    }

    /// 获取审计日志（克隆为 Vec 以兼容外部 API）
    pub async fn audit_log(&self) -> Vec<AuditRecord> {
        self.audit_log.read().await.to_vec()
    }

    /// 清空审计日志
    pub async fn clear_audit(&self) {
        self.audit_log.write().await.clear();
    }

    /// 生成并存储 HMAC 密钥
    ///
    /// CSPRNG 失败时返回 `Err`（C6 修复：调用方决定 fail-fast 或重试）
    pub async fn generate_hmac_key(&self) -> Result<(), getrandom::Error> {
        let key = SecretString::generate(32)?;
        self.store(SecretType::HmacKey, key).await;
        Ok(())
    }

    /// 从环境变量加载 API 密钥
    pub async fn load_from_env(&self, env_var: &str, type_: SecretType) -> Result<(), String> {
        let value = std::env::var(env_var)
            .map_err(|e| format!("env var {env_var} not set: {e}"))?;
        self.store(type_, SecretString::new(value)).await;
        Ok(())
    }
}

impl Default for SecretsManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_string_zeroize() {
        let secret = SecretString::new("my-secret-key");
        assert_eq!(secret.as_str(), Some("my-secret-key"));
        // After drop, memory should be zeroed (can't directly test, but no panic)
    }

    #[test]
    fn test_secret_string_debug() {
        let secret = SecretString::new("my-secret-key");
        let debug = format!("{:?}", secret);
        assert_eq!(debug, "[REDACTED]");
    }

    #[test]
    fn test_secret_generate() {
        let secret = SecretString::generate(32).expect("CSPRNG must work in test");
        assert_eq!(secret.as_bytes().len(), 32);
        // Should be random, not all zeros
        assert!(secret.as_bytes().iter().any(|&b| b != 0));
    }

    #[tokio::test]
    async fn test_secrets_manager_store_and_get() {
        let manager = SecretsManager::new();
        let secret = SecretString::new("api-key-123");
        manager.store(SecretType::ApiKey("deepseek".into()), secret).await;

        let retrieved = manager.get(&SecretType::ApiKey("deepseek".into()), "test").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().as_str(), Some("api-key-123"));
    }

    #[tokio::test]
    async fn test_secrets_manager_audit() {
        let manager = SecretsManager::new();
        manager.store(SecretType::HmacKey, SecretString::generate(32).expect("CSPRNG")).await;

        manager.get(&SecretType::HmacKey, "service-a").await;
        manager.get(&SecretType::HmacKey, "service-b").await;

        let log = manager.audit_log().await;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].accessed_by, "service-a");
        assert_eq!(log[1].accessed_by, "service-b");
    }

    #[tokio::test]
    async fn test_secrets_manager_remove() {
        let manager = SecretsManager::new();
        manager.store(SecretType::ApiKey("test".into()), SecretString::new("key")).await;
        assert!(manager.remove(&SecretType::ApiKey("test".into())).await);
        assert!(!manager.remove(&SecretType::ApiKey("test".into())).await);
    }
}
