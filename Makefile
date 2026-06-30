# Phase 2 起 release pipeline 全部走 GitHub Actions (.github/workflows/release.yml)。
# 本地 Makefile 只保留两个 target:
#   mac-app  - 本地自测出 .app
#   clean    - 清理 build/dist/release/.tmp
# 三平台 release 触发: gh workflow run release.yml -f version=2.0.1
# 详见 docs/build.md。

.PHONY: help mac-app clean

help:
	@echo "Targets:"
	@echo "  mac-app   Build Tauri unsigned macOS .app into dist/mac/ (本地自测用)"
	@echo "  clean     Remove build/, dist/, release/, .release-signing/, .tmp/"
	@echo ""
	@echo "Release: 三平台 release 由 GitHub Actions 出, 不再走本地 Makefile."
	@echo "         手动触发: gh workflow run release.yml -f version=<x.y.z>"
	@echo "         tag 触发: git tag v<x.y.z> && git push --tags"

mac-app:
	npm --prefix frontend ci
	npm --prefix frontend run build
	# 不硬编 CARGO_TARGET_DIR —— 继承 cargo 配置/环境的 target-dir(本机 .cargo/config.toml
	# 可把 main + 所有 worktree 统一到一个 target 避免重复占盘;无配置时退回默认 workspace
	# target)。bundle 落在实际 target 下,用 cargo metadata 定位,不假设 src-tauri/target。
	cargo tauri build --bundles app
	@TARGET_DIR=$$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])'); \
	mkdir -p dist/mac; \
	rm -rf "dist/mac/Codex App Transfer.app"; \
	cp -R "$$TARGET_DIR/release/bundle/macos/Codex App Transfer.app" "dist/mac/Codex App Transfer.app"
	@echo ""
	@echo "✓ Built: dist/mac/Codex App Transfer.app"

clean:
	rm -rf build dist release .release-signing .tmp
