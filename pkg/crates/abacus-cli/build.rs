// build.rs — 注入 git commit hash 到编译产物
//
// 引用关系：main.rs 通过 env!("GIT_HASH") 读取
// 生命周期：每次 cargo build 时执行

fn main() {
    // 注入 git short hash
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_HASH={}", hash);

    // 注入构建时间 (ISO 8601 日期)
    let date = chrono_lite_date();
    println!("cargo:rustc-env=BUILD_DATE={}", date);

    // 仅在 git HEAD 变化时重新运行（而非每次编译）
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/");
}

/// 简单日期生成（不引入 chrono 依赖）
fn chrono_lite_date() -> String {
    std::process::Command::new("date")
        .args(["+%Y-%m-%d"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".into())
}
