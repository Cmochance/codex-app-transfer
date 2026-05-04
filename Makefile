PYTHON  ?= .venv/bin/python
# 唯一版本源: backend/config.py:APP_VERSION
# 优先用环境变量(允许 CI 临时覆盖如 release candidates),否则从 Python 读取。
VERSION ?= $(shell $(PYTHON) -c "from backend.config import APP_VERSION; print(APP_VERSION)" 2>/dev/null || echo 0.0.0)
WIN_IMAGE_TAG   ?= codex-app-transfer-win:latest
LINUX_IMAGE_TAG ?= codex-app-transfer-linux:latest

REPO_FLAG := $(if $(CCDS_REPO),--repo $(CCDS_REPO),)

.PHONY: help mac-app mac-release win-image win-release linux-image linux-release release-bundle release clean

help:
	@echo "Targets:"
	@echo "  mac-app          Build Tauri unsigned macOS .app into dist/mac/(本地自测用)"
	@echo "  mac-release      Build macOS .app/.pkg/.dmg + sign + index in release/"
	@echo "  linux-release    Cross-build Linux x86_64 tarball + onefile via Docker + sign"
	@echo "  win-release      Cross-build Windows portable + onefile + Setup .exe via Docker + sign"
	@echo "  release          mac-release + linux-release + win-release"
	@echo "  release-bundle   Re-run release_assets.py against existing dist/ artifacts"
	@echo "  win-image        Build the Windows builder Docker image"
	@echo "  linux-image      Build the Linux builder Docker image"
	@echo "  clean            Remove build/, dist/, release/, .release-signing/, .tmp/"
	@echo ""
	@echo "Variables: VERSION=$(VERSION), PYTHON=$(PYTHON)"
	@echo "           WIN_IMAGE_TAG=$(WIN_IMAGE_TAG)"
	@echo "           LINUX_IMAGE_TAG=$(LINUX_IMAGE_TAG)"
	@echo "           CCDS_REPO=<owner/repo>  (optional; embeds asset URLs in latest.json)"

mac-app:
	@# Stage 6 起的 Tauri 路径:cargo tauri build → dist/mac/<App>.app
	@# 默认 only `--bundles app`(本地自测不出 dmg/pkg);打分发包走 mac-release
	cargo tauri build --bundles app
	mkdir -p dist/mac
	rm -rf "dist/mac/Codex App Transfer.app"
	cp -R "target/release/bundle/macos/Codex App Transfer.app" "dist/mac/Codex App Transfer.app"
	@echo ""
	@echo "✓ Built: dist/mac/Codex App Transfer.app"

mac-release:
	CCDS_VERSION=$(VERSION) PYTHON_BIN=$(PYTHON) bash macos/build-macos.sh
	$(PYTHON) scripts/release_assets.py --version $(VERSION) --include macos $(REPO_FLAG)

linux-image:
	docker build --platform linux/amd64 -t $(LINUX_IMAGE_TAG) -f docker/linux-builder/Dockerfile .

linux-release:
	IMAGE_TAG=$(LINUX_IMAGE_TAG) bash scripts/build-linux-on-mac.sh $(VERSION)
	$(PYTHON) scripts/release_assets.py --version $(VERSION) --include linux $(REPO_FLAG)

win-image:
	docker build --platform linux/amd64 -t $(WIN_IMAGE_TAG) -f docker/windows-builder/Dockerfile .

win-release:
	IMAGE_TAG=$(WIN_IMAGE_TAG) bash scripts/build-windows-on-mac.sh $(VERSION)
	$(PYTHON) scripts/release_assets.py --version $(VERSION) --include windows $(REPO_FLAG)

release-bundle:
	$(PYTHON) scripts/release_assets.py --version $(VERSION) $(REPO_FLAG)

release: mac-release linux-release win-release release-bundle

clean:
	rm -rf build dist release .release-signing .tmp
