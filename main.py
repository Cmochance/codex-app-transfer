#!/usr/bin/env python3
"""Codex App Transfer - 启动入口"""

import argparse
import atexit
import ctypes
import sys
import threading
import time
import traceback
from pathlib import Path
import webbrowser
from urllib.error import URLError
from urllib.request import urlopen

import uvicorn

from backend.main import (
    create_admin_app,
    desktop_config_target_for_provider,
    register_update_quit_handler,
    _start_proxy_server,
    _stop_proxy_server,
)
from backend import config as cfg
from backend import registry


APP_NAME = "Codex App Transfer"
APP_VERSION = "1.0.1"
TRAY_OPEN_LABEL = "打开 Codex App Transfer"
TRAY_QUIT_LABEL = "退出"
_macos_app_delegate = None
MB_OK = 0x00000000
MB_ICONINFORMATION = 0x00000040
MB_SETFOREGROUND = 0x00010000


def safe_print(message: str):
    """windowed exe 没有控制台时，print 不能影响主流程。"""
    stream = getattr(sys, "stdout", None)
    if not stream:
        return
    try:
        print(message)
    except OSError:
        return


def show_message_box(title: str, message: str) -> bool:
    """显示原生提示框；不可用时返回 False。"""
    try:
        ctypes.windll.user32.MessageBoxW(
            None,
            message,
            title,
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        )
        return True
    except Exception as exc:
        safe_print(f"message box failed: {exc}")
        return False


def show_message_box_async(title: str, message: str):
    """在独立线程弹提示框，避免阻塞托盘菜单回调。"""
    threading.Thread(
        target=show_message_box,
        args=(title, message),
        daemon=True,
    ).start()


def write_crash_log():
    """打包为 windowed exe 后没有控制台，崩溃信息写入本机日志。"""
    try:
        cfg.ensure_config_dir()
        log_path = Path(cfg.CONFIG_DIR) / "cas-crash.log"
        log_path.write_text(traceback.format_exc(), encoding="utf-8")
    except Exception:
        return


def restore_codex_if_enabled(reason: str = "exit") -> bool:
    """根据设置开关 restoreCodexOnExit 决定是否还原 ~/.codex/ 至 apply 之前的状态。

    幂等：无快照 / 开关关闭时直接跳过；任何异常都吞掉（不能让退出/启动流程崩）。
    """
    try:
        if not cfg.get_settings().get("restoreCodexOnExit", True):
            return False
        if not registry.has_snapshot():
            return False
        result = registry.restore_codex_state()
        safe_print(f"[{reason}] restore codex state: {result.get('message', '')}")
        return bool(result.get("success"))
    except Exception as exc:
        safe_print(f"[{reason}] restore codex state failed: {exc}")
        return False


def parse_args():
    """解析启动参数。默认走桌面窗口，浏览器模式只作为备用。"""
    parser = argparse.ArgumentParser(description=APP_NAME)
    parser.add_argument(
        "--browser",
        action="store_true",
        help="Open the system browser instead of the desktop window.",
    )
    parser.add_argument(
        "--server-only",
        action="store_true",
        help="Start the local admin server without opening any UI.",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=None,
        help="Override the admin server port.",
    )
    return parser.parse_args()


def wait_for_admin(url: str, timeout: float = 12.0) -> bool:
    """等待管理后台可访问，避免窗口先打开后白屏。"""
    deadline = time.time() + timeout
    status_url = f"{url}/api/status"

    while time.time() < deadline:
        try:
            with urlopen(status_url, timeout=0.6) as response:
                if response.status < 500:
                    return True
        except (OSError, URLError):
            time.sleep(0.2)

    return False


def build_admin_server(admin_app, port: int) -> uvicorn.Server:
    """创建可由桌面窗口生命周期控制的管理后台服务器。"""
    server_config = uvicorn.Config(
        admin_app,
        host="127.0.0.1",
        port=port,
        log_level="warning",
        access_log=False,
        log_config=None,
    )
    return uvicorn.Server(server_config)


def start_admin_server(admin_app, port: int):
    """后台线程启动管理后台，供 WebView 或浏览器访问。"""
    server = build_admin_server(admin_app, port)
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    return server, thread


def open_browser_when_ready(url: str):
    if wait_for_admin(url):
        webbrowser.open(url)


def _macos_should_quit_from_close_event() -> bool:
    """Best-effort distinction between closing the window and quitting the app."""
    if sys.platform != "darwin":
        return False

    try:
        import AppKit
    except Exception:
        return False

    try:
        event = AppKit.NSApp.currentEvent()
    except Exception:
        return False
    if event is None:
        return True

    try:
        event_window = event.window()
    except Exception:
        event_window = None
    if event_window is None:
        return True

    try:
        if event.type() == AppKit.NSKeyDown:
            chars = str(event.charactersIgnoringModifiers() or "").lower()
            return chars == "q"
    except Exception:
        return False

    return False


def _install_macos_reopen_handler(window, controller):
    """Install a Cocoa app delegate that restores the hidden Dock app window."""
    if sys.platform != "darwin":
        return

    try:
        window.events.shown.wait(10)
        import AppKit
        import Foundation
        from PyObjCTools import AppHelper
        from webview.platforms.cocoa import BrowserView
    except Exception as exc:
        safe_print(f"macOS reopen handler unavailable: {exc}")
        return

    class CCDesktopSwitchAppDelegate(AppKit.NSObject):
        def applicationShouldTerminate_(self, app):
            should_close = True
            try:
                for instance in list(BrowserView.instances.values()):
                    should_close = should_close and BrowserView.should_close(instance.pywebview_window)
            except Exception:
                return Foundation.YES
            return Foundation.YES if should_close else Foundation.NO

        def applicationSupportsSecureRestorableState_(self, app):
            return Foundation.YES

        def applicationShouldHandleReopen_hasVisibleWindows_(self, app, has_visible_windows):
            if controller.window_hidden or not bool(has_visible_windows):
                controller.show_window()
            return Foundation.YES

    delegate = CCDesktopSwitchAppDelegate.alloc().init().retain()

    def set_delegate():
        global _macos_app_delegate
        _macos_app_delegate = delegate
        AppKit.NSApplication.sharedApplication().setDelegate_(delegate)

    AppHelper.callAfter(set_delegate)


class DesktopTrayController:
    """系统托盘控制器：关闭窗口时隐藏，托盘菜单里显式退出。"""

    def __init__(self, window, icon_path: Path):
        self.window = window
        self.icon_path = Path(icon_path)
        self.icon = None
        self.thread = None
        self.pystray = None
        self.exit_requested = False
        self._notified = False
        self.window_hidden = False

    def start(self) -> bool:
        """启动系统托盘图标。依赖缺失时返回 False，不影响主窗口打开。"""
        if sys.platform == "darwin":
            safe_print("system tray disabled on macOS: AppKit must run on the main thread")
            return False

        try:
            import pystray
            from PIL import Image
        except Exception as exc:
            safe_print(f"system tray unavailable: {exc}")
            return False

        try:
            self.pystray = pystray
            image = Image.open(self.icon_path)
            self.icon = pystray.Icon(APP_NAME, image, APP_NAME, self.build_menu())
            self.thread = threading.Thread(target=self.icon.run, daemon=True)
            self.thread.start()
            return True
        except Exception as exc:
            safe_print(f"system tray failed: {exc}")
            return False

    def build_menu(self):
        """构建托盘菜单，包含 provider 快速切换项。"""
        if not self.pystray:
            return None
        items = [
            self.pystray.MenuItem(TRAY_OPEN_LABEL, self.show_window, default=True),
            self.pystray.Menu.SEPARATOR,
        ]
        items.extend(self.provider_menu_items())
        items.extend([
            self.pystray.Menu.SEPARATOR,
            self.pystray.MenuItem(TRAY_QUIT_LABEL, self.quit_app),
        ])
        return self.pystray.Menu(*items)

    def provider_menu_items(self):
        """返回 provider 切换菜单项。"""
        if not self.pystray:
            return []
        config = cfg.load_config()
        active_id = config.get("activeProvider")
        providers = config.get("providers", [])
        if not providers:
            return [self.pystray.MenuItem("暂无提供商", None, enabled=False)]

        items = [self.pystray.MenuItem("切换提供商", None, enabled=False)]
        for provider in providers:
            provider_id = provider.get("id")
            name = provider.get("name", "Unnamed Provider")
            items.append(self.pystray.MenuItem(
                name,
                self._make_provider_switcher(provider_id),
                checked=lambda item, pid=provider_id: cfg.load_config().get("activeProvider") == pid,
            ))
        return items

    def _make_provider_switcher(self, provider_id: str):
        def switch(icon=None, item=None):
            self.switch_provider(provider_id)
        return switch

    def switch_provider(self, provider_id: str) -> bool:
        """从托盘菜单切换默认 provider。"""
        if not provider_id:
            return False
        previous_id = cfg.load_config().get("activeProvider")
        if not cfg.set_active_provider(provider_id):
            return False
        provider = cfg.get_provider(provider_id)
        desktop_message = ""
        desktop_synced = False
        try:
            if provider:
                settings = cfg.get_settings()
                target = desktop_config_target_for_provider(provider, settings)
                result = registry.apply_config(
                    target["baseUrl"],
                    gateway_api_key=target["apiKey"],
                    provider=target["provider"],
                    providers=target["providers"],
                    expose_all=target["exposeAll"],
                    auth_scheme=target["authScheme"],
                    gateway_headers=target["gatewayHeaders"],
                )
                if target.get("requiresProxy"):
                    _start_proxy_server(settings.get("proxyPort", 18080))
                desktop_synced = bool(result.get("success"))
                desktop_message = "，Codex CLI 配置已同步，重启终端后生效" if result.get("success") else "，请重新一键生成 Codex CLI 配置"
        except Exception as exc:
            safe_print(f"sync desktop config failed: {exc}")
            desktop_message = "，请重新一键生成 Codex CLI 配置"
        self.refresh_menu()
        try:
            if self.icon and provider:
                self.icon.notify(f"已切换到 {provider.get('name', provider_id)}{desktop_message}", APP_NAME)
        except Exception:
            pass
        return True

    def show_desktop_restart_dialog(self, provider: dict, desktop_synced: bool = False):
        """托盘切换 provider 后给出明确重启提醒。"""
        provider_name = provider.get("name") or "当前提供商"
        sync_line = (
            "本工具已同步 Codex CLI 模型配置。"
            if desktop_synced
            else "如果 Codex CLI 已经配置过本工具，模型会在下次终端启动后生效。"
        )
        message = (
            f"已切换到：{provider_name}\n\n"
            f"{sync_line}\n"
            "请完全退出并重新打开终端，然后再使用新模型。"
        )
        show_message_box_async("需要重启终端", message)

    def refresh_menu(self):
        """provider 变化后刷新托盘菜单。"""
        if not self.icon or not self.pystray:
            return
        try:
            self.icon.menu = self.build_menu()
            if hasattr(self.icon, "update_menu"):
                self.icon.update_menu()
        except Exception as exc:
            safe_print(f"refresh tray menu failed: {exc}")

    def handle_window_closing(self):
        """pywebview closing 事件：返回 False 表示取消关闭。"""
        if self.exit_requested or _macos_should_quit_from_close_event():
            return None

        self.hide_window()
        self.notify_hidden()
        return False

    def hide_window(self):
        self.window.hide()
        self.window_hidden = True

    def show_window(self, icon=None, item=None):
        try:
            self.window.show()
            self.window.restore()
            self.window_hidden = False
        except Exception as exc:
            safe_print(f"show window failed: {exc}")

    def notify_hidden(self):
        if self._notified or not self.icon:
            return
        self._notified = True
        try:
            self.icon.notify(
                "程序仍在后台运行。右键托盘图标可打开或退出。",
                APP_NAME,
            )
        except Exception:
            return

    def quit_app(self, icon=None, item=None):
        self.exit_requested = True
        try:
            self.window.destroy()
        except Exception as exc:
            safe_print(f"quit failed: {exc}")

    def stop(self):
        if not self.icon:
            return
        try:
            self.icon.stop()
        except Exception:
            return


def open_desktop_window(url: str) -> bool:
    """打开原生桌面窗口。失败时返回 False，让调用方退回浏览器模式。"""
    try:
        import webview
    except Exception as exc:
        safe_print(f"pywebview unavailable, fallback to browser: {exc}")
        return False

    try:
        window = webview.create_window(
            APP_NAME,
            url,
            width=1240,
            height=820,
            min_size=(980, 680),
            text_select=True,
        )
        tray = DesktopTrayController(
            window,
            Path(__file__).resolve().parent / "frontend" / "assets" / "app-icon.png",
        )

        def request_quit_for_update():
            if sys.platform == "darwin":
                try:
                    from PyObjCTools import AppHelper
                    AppHelper.callAfter(tray.quit_app)
                    return
                except Exception:
                    pass
            tray.quit_app()

        tray_started = tray.start()
        if tray_started or sys.platform == "darwin":
            window.events.closing += tray.handle_window_closing
        if tray_started:
            window.events.closed += tray.stop

        menu = []
        if sys.platform == "darwin":
            from webview.menu import Menu, MenuAction, MenuSeparator

            menu = [
                Menu("Window", [
                    MenuAction("Show Codex App Transfer", tray.show_window),
                    MenuSeparator(),
                    MenuAction("Quit Codex App Transfer", tray.quit_app),
                ]),
            ]

        register_update_quit_handler(request_quit_for_update)
        try:
            if sys.platform == "darwin":
                webview.start(
                    func=_install_macos_reopen_handler,
                    args=(window, tray),
                    debug=False,
                    menu=menu,
                )
            else:
                webview.start(debug=False, menu=menu)
        finally:
            register_update_quit_handler(None)
        return True
    except Exception as exc:
        safe_print(f"desktop window failed, fallback to browser: {exc}")
        return False


def run_browser_mode(admin_app, admin_port: int, open_ui: bool = True):
    url = f"http://127.0.0.1:{admin_port}"
    if open_ui:
        threading.Thread(target=open_browser_when_ready, args=(url,), daemon=True).start()

    safe_print(f"""
╔══════════════════════════════════════════╗
║       {APP_NAME} v{APP_VERSION}          ║
║                                          ║
║  管理后台: {url}     ║
║                                          ║
║  按 Ctrl+C 停止                          ║
╚══════════════════════════════════════════╝
    """)

    uvicorn.run(
        admin_app,
        host="127.0.0.1",
        port=admin_port,
        log_level="warning",
        access_log=False,
        log_config=None,
    )


def run_desktop_mode(admin_app, admin_port: int):
    url = f"http://127.0.0.1:{admin_port}"
    server, server_thread = start_admin_server(admin_app, admin_port)

    try:
        if not wait_for_admin(url):
            safe_print(f"admin server is not ready, fallback to browser: {url}")
            webbrowser.open(url)
            while server_thread.is_alive() and not server.should_exit:
                time.sleep(0.5)
            return

        if not open_desktop_window(url):
            webbrowser.open(url)
            while server_thread.is_alive() and not server.should_exit:
                time.sleep(0.5)
    finally:
        server.should_exit = True
        _stop_proxy_server()
        restore_codex_if_enabled(reason="desktop-finally")


def main():
    args = parse_args()

    # 确保配置目录存在
    cfg.ensure_config_dir()

    # 启动兜底（A3）：上一会话若异常退出（SIGKILL / 断电 / 闪退）会留下快照,
    # 设置开关开启时静默还原一次,保证「不开应用 → ~/.codex/ 即原配置」语义。
    restore_codex_if_enabled(reason="startup")

    # 注册退出钩子（B2）：覆盖正常退出路径（托盘退出、Ctrl+C、窗口关闭、
    # webview.start 返回等）。SIGKILL 走不到这里,由下次启动 startup 钩子兜底。
    atexit.register(restore_codex_if_enabled, "atexit")

    # 读取设置
    settings = cfg.get_settings()
    admin_port = args.port or settings.get("adminPort", 18081)
    proxy_port = settings.get("proxyPort", 18080)
    auto_start_proxy = settings.get("autoStart", False)

    # 如果开启了自动启动代理
    if auto_start_proxy:
        safe_print(f"  自动启动代理 (端口 {proxy_port})...")
        _start_proxy_server(proxy_port)

    # 创建管理后台应用
    admin_app = create_admin_app()

    if args.server_only:
        run_browser_mode(admin_app, admin_port, open_ui=False)
    elif args.browser:
        run_browser_mode(admin_app, admin_port, open_ui=True)
    else:
        run_desktop_mode(admin_app, admin_port)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        write_crash_log()
        raise
