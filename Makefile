INSTALL_BIN  := /usr/local/bin
CONFIG_DIR   := /etc/toaster-talk
SYSTEMD_DIR  := /etc/systemd/system
SERVICE_NAME := toaster-talk

DEV_STATE    := target/dev-instance
DEV_TMP      := /tmp/toaster-talk-dev
DEV_SOCK     := $(DEV_TMP)/mgmt.sock
DEV_CLI      := target/release/toaster-talk-cli --socket $(DEV_SOCK) --token-dir $(DEV_STATE)/tokens
# Scratch space for anything the orchestrator/probes create during a manual-test
# run (throwaway scripts, curl output, ad-hoc dumps) — never the repo root.
# Gitignored via /target, wiped every dev-down alongside the rest of dev state.
DEV_SCRATCH  := $(DEV_STATE)/scratch

.PHONY: build install uninstall dev-up dev-down dev-status manual-test \
       _install-linux _uninstall-linux _install-macos _uninstall-macos

build:
	cargo build --release

# ── Manual-test dev instance (isolated from prod :6669) ───────────────────────
# Runs a throwaway server from the repo on :6678 with all state under
# $(DEV_TMP) and $(DEV_STATE) (ephemeral / gitignored). See
# toaster-talk-server/tests/MANUAL-TEST.md.

dev-up: build
	@mkdir -p $(DEV_TMP) $(DEV_STATE)/tokens $(DEV_SCRATCH)
	@test -s $(DEV_STATE)/master.key || { \
		head -c 48 /dev/urandom | base64 | tr -d '\n' > $(DEV_STATE)/master.key; \
		chmod 600 $(DEV_STATE)/master.key; echo "generated dev master key"; }
	@if [ -f $(DEV_STATE)/dev.pid ] && kill -0 $$(cat $(DEV_STATE)/dev.pid) 2>/dev/null; then \
		echo "dev instance already running (PID $$(cat $(DEV_STATE)/dev.pid))"; exit 0; fi
	@rm -f $(DEV_TMP)/rooms.yaml
	@nohup target/release/toaster-talk-server --config deploy/config.dev.toml \
		> $(DEV_STATE)/dev.log 2>&1 & echo $$! > $(DEV_STATE)/dev.pid
	@echo "dev instance starting (PID $$(cat $(DEV_STATE)/dev.pid)); waiting for :6678 ..."
	@for i in $$(seq 1 40); do \
		if $(DEV_CLI) lsrm >/dev/null 2>&1; then \
			echo "dev instance UP on :6678 (mgmt: $(DEV_SOCK))"; exit 0; fi; \
		sleep 0.25; \
	done; echo "ERROR: dev instance not ready — see $(DEV_STATE)/dev.log"; exit 1

dev-down:
	@if [ -f $(DEV_STATE)/dev.pid ]; then \
		kill $$(cat $(DEV_STATE)/dev.pid) 2>/dev/null || true; \
		rm -f $(DEV_STATE)/dev.pid; echo "dev instance stopped"; \
	else echo "no dev instance running"; fi
	@rm -rf $(DEV_TMP) $(DEV_STATE)/tokens $(DEV_SCRATCH)
	@echo "dev state wiped (master.key retained at $(DEV_STATE)/master.key)"

dev-status:
	@if [ -f $(DEV_STATE)/dev.pid ] && kill -0 $$(cat $(DEV_STATE)/dev.pid) 2>/dev/null; then \
		echo "running (PID $$(cat $(DEV_STATE)/dev.pid))"; $(DEV_CLI) lsrm; \
	else echo "not running"; fi

# Operator entry point: launch a dedicated, UNATTENDED (headless) session that sees
# ONLY the dev MCP (strict), with the orchestrator brief. Runs in the background so
# you can do other things; tears the instance down when finished. Run from a plain
# shell, not inside a Claude session. Override BRIEF to change the instruction.
BRIEF ?= Run every scenario in toaster-talk-server/tests/MANUAL-TEST.md against the dev instance and report pass/fail versus the documented dump signatures.
# The Bash(...) entries MUST stay single-quoted: unquoted parentheses are a syntax
# error in both dash and bash when the recipe line is expanded, which aborts the
# target before claude launches. The quotes reach the shell literally and are
# stripped there, so claude receives the bare `Bash(...:*)` permission strings.
MANUAL_TOOLS := Task Read ToolSearch SendMessage \
	mcp__ToasterTalkDev__* \
	'Bash(target/release/toaster-talk-cli:*)' 'Bash(yq:*)'
manual-test: dev-up
	claude --mcp-config deploy/dev-mcp.json --strict-mcp-config \
		--append-system-prompt-file toaster-talk-server/tests/orchestrator-brief.md \
		--allowedTools $(MANUAL_TOOLS) \
		-p "$(BRIEF)"; $(MAKE) dev-down

# ── OS detection ──────────────────────────────────────────────────────────────
UNAME_S      := $(shell uname -s)
SERVICE_LABEL := com.toaster-talk
LAUNCHD_PLIST := /Library/LaunchDaemons/$(SERVICE_LABEL).plist
DATA_DIR     := /var/lib/toaster-talk
# NOT /var/run: macOS clears it on every boot and launchd has no systemd
# RuntimeDirectory= equivalent, so the socket's parent would vanish and the
# daemon would fail to bind its mgmt listener after a reboot. /usr/local/var/run
# persists.
RUN_DIR      := /usr/local/var/run/toaster-talk
LOG_DIR      := /var/log

install:
	@test -f target/release/toaster-talk-server || \
		{ echo "Run 'make build' first"; exit 1; }
ifeq ($(UNAME_S),Darwin)
	$(MAKE) _install-macos
else
	$(MAKE) _install-linux
endif

uninstall:
ifeq ($(UNAME_S),Darwin)
	$(MAKE) _uninstall-macos
else
	$(MAKE) _uninstall-linux
endif

# ── Linux (systemd) ──────────────────────────────────────────────────────────
_install-linux:
	@id toaster-talk >/dev/null 2>&1 || \
		useradd --system --no-create-home --shell /usr/sbin/nologin toaster-talk
	install -m 755 target/release/toaster-talk-server $(INSTALL_BIN)/
	install -m 755 target/release/toaster-talk-cli    $(INSTALL_BIN)/
	install -m 644 deploy/toaster-talk.service $(SYSTEMD_DIR)/$(SERVICE_NAME).service
	@mkdir -p $(CONFIG_DIR)
	@if [ ! -f $(CONFIG_DIR)/config.toml ]; then \
		install -m 644 deploy/config.example.toml $(CONFIG_DIR)/config.toml; \
		echo "Default config written to $(CONFIG_DIR)/config.toml"; \
	fi
	@if [ ! -f $(CONFIG_DIR)/master.secret ]; then \
		head -c 48 /dev/urandom | base64 > $(CONFIG_DIR)/master.secret; \
		chmod 600 $(CONFIG_DIR)/master.secret; \
		chown toaster-talk $(CONFIG_DIR)/master.secret; \
		echo "Generated master secret at $(CONFIG_DIR)/master.secret"; \
	fi
	systemctl daemon-reload
	@echo "Installed. Enable with: systemctl enable --now $(SERVICE_NAME)"

_uninstall-linux:
	@systemctl stop $(SERVICE_NAME) 2>/dev/null || true
	@systemctl disable $(SERVICE_NAME) 2>/dev/null || true
	rm -f $(INSTALL_BIN)/toaster-talk-server
	rm -f $(INSTALL_BIN)/toaster-talk-cli
	rm -f $(SYSTEMD_DIR)/$(SERVICE_NAME).service
	systemctl daemon-reload

# ── macOS (launchd) ──────────────────────────────────────────────────────────
_install-macos:
	@mkdir -p $(INSTALL_BIN)
	install -m 755 target/release/toaster-talk-server $(INSTALL_BIN)/
	install -m 755 target/release/toaster-talk-cli    $(INSTALL_BIN)/
	@mkdir -p $(CONFIG_DIR) $(DATA_DIR) $(RUN_DIR)
	@if [ ! -f $(CONFIG_DIR)/config.toml ]; then \
		install -m 644 deploy/config.example.toml $(CONFIG_DIR)/config.toml; \
		sed -i '' 's|/run/toaster-talk/mgmt.sock|$(RUN_DIR)/mgmt.sock|' $(CONFIG_DIR)/config.toml; \
		echo "Default config written to $(CONFIG_DIR)/config.toml"; \
	elif grep -Fq 'mgmt_socket = "/run/toaster-talk/mgmt.sock"' $(CONFIG_DIR)/config.toml; then \
		sed -i '' 's|mgmt_socket = "/run/toaster-talk/mgmt.sock"|mgmt_socket = "$(RUN_DIR)/mgmt.sock"|' $(CONFIG_DIR)/config.toml; \
		echo "Updated legacy macOS management socket path in $(CONFIG_DIR)/config.toml"; \
	elif grep -Fq 'mgmt_socket = "/var/run/toaster-talk/mgmt.sock"' $(CONFIG_DIR)/config.toml; then \
		sed -i '' 's|mgmt_socket = "/var/run/toaster-talk/mgmt.sock"|mgmt_socket = "$(RUN_DIR)/mgmt.sock"|' $(CONFIG_DIR)/config.toml; \
		echo "Updated legacy macOS management socket path in $(CONFIG_DIR)/config.toml"; \
	fi
	@if [ ! -f $(CONFIG_DIR)/master.secret ]; then \
		head -c 48 /dev/urandom | base64 > $(CONFIG_DIR)/master.secret; \
		chmod 600 $(CONFIG_DIR)/master.secret; \
		echo "Generated master secret at $(CONFIG_DIR)/master.secret"; \
	fi
	@echo '<?xml version="1.0" encoding="UTF-8"?>' > $(LAUNCHD_PLIST)
	@echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' >> $(LAUNCHD_PLIST)
	@echo '<plist version="1.0">' >> $(LAUNCHD_PLIST)
	@echo '<dict>' >> $(LAUNCHD_PLIST)
	@echo '    <key>Label</key>' >> $(LAUNCHD_PLIST)
	@echo '    <string>$(SERVICE_LABEL)</string>' >> $(LAUNCHD_PLIST)
	@echo '    <key>ProgramArguments</key>' >> $(LAUNCHD_PLIST)
	@echo '    <array>' >> $(LAUNCHD_PLIST)
	@echo '        <string>$(INSTALL_BIN)/toaster-talk-server</string>' >> $(LAUNCHD_PLIST)
	@echo '        <string>--config</string>' >> $(LAUNCHD_PLIST)
	@echo '        <string>$(CONFIG_DIR)/config.toml</string>' >> $(LAUNCHD_PLIST)
	@echo '    </array>' >> $(LAUNCHD_PLIST)
	@echo '    <key>EnvironmentVariables</key>' >> $(LAUNCHD_PLIST)
	@echo '    <dict>' >> $(LAUNCHD_PLIST)
	@echo '        <key>RUST_LOG</key>' >> $(LAUNCHD_PLIST)
	@echo '        <string>info</string>' >> $(LAUNCHD_PLIST)
	@echo '    </dict>' >> $(LAUNCHD_PLIST)
	@echo '    <key>RunAtLoad</key>' >> $(LAUNCHD_PLIST)
	@echo '    <true/>' >> $(LAUNCHD_PLIST)
	@echo '    <key>KeepAlive</key>' >> $(LAUNCHD_PLIST)
	@echo '    <true/>' >> $(LAUNCHD_PLIST)
	@echo '    <key>StandardOutPath</key>' >> $(LAUNCHD_PLIST)
	@echo '    <string>$(LOG_DIR)/toaster-talk.log</string>' >> $(LAUNCHD_PLIST)
	@echo '    <key>StandardErrorPath</key>' >> $(LAUNCHD_PLIST)
	@echo '    <string>$(LOG_DIR)/toaster-talk.err</string>' >> $(LAUNCHD_PLIST)
	@echo '</dict>' >> $(LAUNCHD_PLIST)
	@echo '</plist>' >> $(LAUNCHD_PLIST)
	@echo "Installed. Start with: sudo launchctl bootstrap system $(LAUNCHD_PLIST)"

_uninstall-macos:
	@launchctl bootout system/$(SERVICE_LABEL) 2>/dev/null || true
	rm -f $(INSTALL_BIN)/toaster-talk-server
	rm -f $(INSTALL_BIN)/toaster-talk-cli
	rm -f $(LAUNCHD_PLIST)
	@echo "Uninstalled. Config and data left in $(CONFIG_DIR) and $(DATA_DIR)."
