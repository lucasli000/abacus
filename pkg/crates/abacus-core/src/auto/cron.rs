//! CronScheduler — 简易 cron 调度器
//!
//! ## 依赖
//! - 无外部 cron 库（简易 minute-resolution 匹配）
//!
//! ## 引用关系
//! - 被 `AutoEngine::register_cron()` 注册任务
//! - `tick()` 由外部 1-minute 循环调用，返回到期的 pipeline_id 列表
//!
//! ## 生命周期
//! 随 AutoEngine 创建，entries 长期存活直到显式移除

/// 简易 cron 调度器
///
/// ## 格式
/// `"*/5 * * * *"` — 每 5 分钟
/// `"0 * * * *"` — 每小时整点
///
/// ## 运行方式
/// 外部每分钟调用 `tick()`，返回当前分钟匹配的 pipeline_id 列表。
/// 调用方负责实际执行（通常是 AutoEngine::fire 或直接 pipeline.run）。
pub struct CronScheduler {
    entries: Vec<CronEntry>,
}

struct CronEntry {
    expression: String,
    pipeline_id: String,
    /// 上次触发时间（分钟精度，防止同分钟重复触发）
    last_fired_minute: Option<i64>,
}

impl CronScheduler {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn add(&mut self, expression: &str, pipeline_id: String) {
        self.entries.push(CronEntry {
            expression: expression.to_string(),
            pipeline_id,
            last_fired_minute: None,
        });
    }

    pub fn remove(&mut self, pipeline_id: &str) {
        self.entries.retain(|e| e.pipeline_id != pipeline_id);
    }

    pub fn entries(&self) -> Vec<(String, String)> {
        self.entries.iter().map(|e| (e.expression.clone(), e.pipeline_id.clone())).collect()
    }

    /// 检查当前时刻是否有到期任务。
    /// 返回本分钟应触发的 pipeline_id 列表。
    /// 调用方应每 60 秒调用一次。
    pub fn tick(&mut self) -> Vec<String> {
        let now = chrono::Utc::now();
        let current_minute = now.timestamp() / 60;
        let mut due = Vec::new();

        for entry in &mut self.entries {
            // 防止同分钟重复触发
            if entry.last_fired_minute == Some(current_minute) {
                continue;
            }
            if cron_matches(&entry.expression, &now) {
                entry.last_fired_minute = Some(current_minute);
                due.push(entry.pipeline_id.clone());
            }
        }
        due
    }
}

impl Default for CronScheduler {
    fn default() -> Self { Self::new() }
}

/// 简易 cron 表达式匹配（5 字段: minute hour dom month dow）
///
/// 支持: `*`, `*/N`, 具体数字。不支持范围（`1-5`）或列表（`1,3,5`）。
/// 对于 V0.2 的 auto 功能足够，复杂表达式可后续引入 cron 库。
fn cron_matches(expr: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    use chrono::Timelike;
    use chrono::Datelike;

    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }

    let minute = now.minute();
    let hour = now.hour();
    let dom = now.day();
    let month = now.month();
    let dow = now.weekday().num_days_from_sunday(); // 0=Sun, 6=Sat

    field_matches(fields[0], minute)
        && field_matches(fields[1], hour)
        && field_matches(fields[2], dom)
        && field_matches(fields[3], month)
        && field_matches(fields[4], dow)
}

/// 匹配单个 cron 字段
fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(divisor) = field.strip_prefix("*/") {
        if let Ok(d) = divisor.parse::<u32>() {
            return d > 0 && value.is_multiple_of(d);
        }
        return false;
    }
    if let Ok(exact) = field.parse::<u32>() {
        return value == exact;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_field_matches() {
        assert!(field_matches("*", 42));
        assert!(field_matches("*/5", 0));
        assert!(field_matches("*/5", 15));
        assert!(!field_matches("*/5", 3));
        assert!(field_matches("30", 30));
        assert!(!field_matches("30", 15));
    }

    #[test]
    fn test_cron_matches() {
        // "*/5 * * * *" should match at minute 0, 5, 10, ...
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 22, 10, 15, 0).unwrap();
        assert!(cron_matches("*/5 * * * *", &t));

        let t2 = chrono::Utc.with_ymd_and_hms(2026, 5, 22, 10, 13, 0).unwrap();
        assert!(!cron_matches("*/5 * * * *", &t2));

        // "0 9 * * *" should match at 09:00
        let t3 = chrono::Utc.with_ymd_and_hms(2026, 5, 22, 9, 0, 0).unwrap();
        assert!(cron_matches("0 9 * * *", &t3));
    }

    #[test]
    fn test_tick_dedup() {
        let mut sched = CronScheduler::new();
        sched.add("* * * * *", "always".into());
        // First tick should fire
        let due = sched.tick();
        assert_eq!(due, vec!["always"]);
        // Immediate second tick same minute should NOT fire
        let due2 = sched.tick();
        assert!(due2.is_empty());
    }
}
