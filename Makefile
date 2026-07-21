# Smelt 打包与开发入口。打包重活在 scripts/package-mac.sh 里。
# GUI 会拉起同目录的 smeltd；只编 GUI 会留下过期/缺失的守护，表现为
# 「新建终端 / 打开项目没反应」。
BIN := smelt
DAEMON := smeltd

.PHONY: help build run icon dist dist-build clean remote-web remote-web-dev

help: ## 显示可用命令
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## 编译 release 二进制（GUI + 守护 + 跨网 bridge）
	cargo build --release --bin $(BIN) --bin $(DAEMON) --bin smelt-bridge

run: ## 本地直接跑 GUI（开发用；先保证 smeltd / bridge 同编）
	cargo build --bin $(BIN) --bin $(DAEMON) --bin smelt-bridge
	cargo run --bin $(BIN)

remote-web: ## 构建远程 H5（Preact CLI 面板 → remote-web/dist）
	cd remote-web && npm install && npm run build

remote-web-dev: ## 远程 H5 热更新（需另开 gateway --port 18765）
	cd remote-web && npm run dev

icon: ## 生成 app 图标（assets/AppIcon.icns）
	./scripts/make-icon.sh

dist: remote-web ## 用已有 release 产物打包 app + dmg（先确保 H5 已构建）
	./scripts/package-mac.sh

dist-build: remote-web ## 先编 H5 + release 再打包（一步到位）
	./scripts/package-mac.sh --build

install: ## 安装 dist/Smelt.app 到 /Applications（先 handoff 守护到 ~/.smelt/bin，再换包）
	@# 绝不能 cp/ditto 覆盖已存在的 .app：同 inode 改写已签名二进制后，
	@# 守护 upgrade 的 exec 会被 macOS 内核直接 SIGKILL（无日志无崩溃报告）。
	@# 更关键：若 smeltd 仍跑在 App 包内，rm -rf 会直接杀掉守护 → 全部会话死亡 →
	@# Claude/Grok 对话被「重新初始化」。必须先把守护迁到 ~/.smelt/bin。
	@test -d dist/Smelt.app || { echo "✗ 无 dist/Smelt.app，先 make dist 或 dist-build"; exit 1; }
	@./scripts/handoff-daemon-to-managed.sh dist/Smelt.app/Contents/MacOS/smeltd || \
		echo "⚠ handoff 未成功（守护可能没在跑或过旧）；若正在用会话，请先开 GUI 让它自动迁移"
	rm -rf /Applications/Smelt.app
	ditto dist/Smelt.app /Applications/Smelt.app
	@# 装完再同步一次 managed 文件（不强制 handoff：守护应已在 managed）
	@mkdir -p "$$HOME/.smelt/bin"
	@cp -f dist/Smelt.app/Contents/MacOS/smeltd "$$HOME/.smelt/bin/smeltd.next"
	@chmod 755 "$$HOME/.smelt/bin/smeltd.next"
	@mv -f "$$HOME/.smelt/bin/smeltd.next" "$$HOME/.smelt/bin/smeltd"
	@echo "✅ 已安装 /Applications/Smelt.app（smeltd 常驻 $$HOME/.smelt/bin/smeltd）"

clean: ## 清理 dist/ 产物
	rm -rf dist
