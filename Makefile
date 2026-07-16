# Smelt 打包与开发入口。打包重活在 scripts/package-mac.sh 里。
BIN := workspace

.PHONY: help build run icon dist dist-build clean remote-web remote-web-dev

help: ## 显示可用命令
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## 编译 release 二进制
	cargo build --release --bin $(BIN)

run: ## 本地直接跑 GUI（开发用）
	cargo run --release --bin $(BIN)

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

install: ## 安装 dist/Smelt.app 到 /Applications（必须先删再拷！）
	@# 绝不能 cp/ditto 覆盖已存在的 .app：同 inode 改写已签名二进制后，
	@# 守护 upgrade 的 exec 会被 macOS 内核直接 SIGKILL（无日志无崩溃报告），
	@# 之后每次 exec 都死，表现为「装完新版守护反复无声死亡」。先 rm 换 inode。
	rm -rf /Applications/Smelt.app
	ditto dist/Smelt.app /Applications/Smelt.app
	@echo "✅ 已安装 /Applications/Smelt.app"

clean: ## 清理 dist/ 产物
	rm -rf dist
