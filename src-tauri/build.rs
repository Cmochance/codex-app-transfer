fn main() {
    // **关键**:`tauri_build::build()` 默认不把 ../frontend 加进 cargo rerun-if-changed,
    // 导致前端代码改了 binary 不重 build。实测 2026-05-10 用户截图显示前端协议名 fallback
    // 错误显示 "OpenAI Chat" 而不是 "Gemini Native",root cause 就是 binary 用的是
    // 5月9日的旧版本(cargo 没探测到 frontend 改动)。
    //
    // 显式声明 frontend 产物 + presets_data.json(embed 进 binary)→ 触发 rerun build。
    // include_dir! 嵌入的是 ../frontend/dist(Vite 产物),只 watch dist 即可(改 frontend/src
    // 需 `npm run build` 产新 dist 才影响 binary);避免 watch ../frontend 整目录把 node_modules
    // 也纳入、每次 npm 操作触发重编。
    println!("cargo:rerun-if-changed=../frontend/dist");
    println!("cargo:rerun-if-changed=../crates/registry/src/presets_data.json");

    // frontend/dist 是 gitignored 构建产物。fresh checkout / `make clean` 后裸
    // `cargo build`/`cargo check`/`cargo tauri dev` 会因 static_files.rs 的 include_dir!
    // (编译期展开)找不到 dist 而 panic。build script 先于 crate 编译执行,这里兜底创建
    // 占位 index.html 保证编译通过;真正前端产物由 `npm --prefix frontend run build` 生成
    // (Makefile mac-app / CI rust-tauri-check / release.yml 均已在 cargo 前显式 build)。
    {
        use std::path::Path;
        let index = Path::new("../frontend/dist/index.html");
        if !index.exists() {
            let _ = std::fs::create_dir_all("../frontend/dist");
            let _ = std::fs::write(
                index,
                "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
                 <title>Codex App Transfer</title></head>\
                 <body style=\"font-family:-apple-system,system-ui,sans-serif;padding:2rem;color:#1d1d1f\">\
                 <h2>前端未构建 / Frontend not built</h2>\
                 <p>运行 <code>npm --prefix frontend run build</code> 生成 frontend/dist 后重新编译。</p>\
                 </body></html>",
            );
        }
    }

    // 让 updateUrl 默认值“跟随当前发布仓库”（任务 1）。
    // - CI release 里通过 GITHUB_REPOSITORY 注入真实 owner/repo，binary 里 baked 的
    //   默认 latest.json URL 就指向该仓库的 releases。
    // - 本地 dev / 普通 cargo build 没有该 env 时，fallback 到 Cmochance（统一为官方源）。
    // - 这样 fork 的人只要复用同样的 release workflow + xtask，就能自动得到正确的更新源。
    let repo = std::env::var("CODEX_APP_TRANSFER_REPO")
        .unwrap_or_else(|_| "Cmochance/codex-app-transfer".to_string());
    let update_url = format!(
        "https://github.com/{}/releases/latest/download/latest.json",
        repo
    );
    println!(
        "cargo:rustc-env=CODEX_APP_TRANSFER_DEFAULT_UPDATE_URL={}",
        update_url
    );
    println!("cargo:rerun-if-env-changed=CODEX_APP_TRANSFER_REPO");

    tauri_build::build()
}
