# Smelt 打包与开发入口。打包重活在 scripts/package-mac.sh 里。
BIN := workspace

.PHONY: help build run icon dist dist-build clean

help: ## 显示可用命令
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## 编译 release 二进制
	cargo build --release --bin $(BIN)

run: ## 本地直接跑 GUI（开发用）
	cargo run --release --bin $(BIN)

icon: ## 生成 app 图标（assets/AppIcon.icns）
	./scripts/make-icon.sh

dist: ## 用已有 release 产物打包 app + dmg + zip
	./scripts/package-mac.sh

dist-build: ## 先编译再打包（一步到位）
	./scripts/package-mac.sh --build

clean: ## 清理 dist/ 产物
	rm -rf dist
