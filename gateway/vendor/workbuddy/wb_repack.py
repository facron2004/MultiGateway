# -*- coding: utf-8 -*-
"""wb_repack.py — 上游 Release 检测、源码拉取、桌面端适配与自动重新打包模块。

功能概要：
1. 检测 GitHub 上游官方仓库 (ardeyouxipianyi/workbuddy2api-hub) 的最新 Release 发布情况。
2. 当发现新版本时（或用户手动触发时），拉取/下载最新上游代码并自动保持桌面端定制项（pywebview API、窗口标题栏、账号/日志路径隔离）。
3. 调用 PyInstaller 重新编译打包 WorkBuddy.exe，捕获完整编译器日志并实时推送给前端。
4. 编译完成后安全替换正在运行的 WorkBuddy.exe（支持 Windows 运行中文件重命名并热替换机制），并提供一键重启能力。
"""

import os
import sys
import json
import time
import shutil
import zipfile
import threading
import subprocess
import urllib.request
import urllib.error

from . import wb_updates

# 上游仓库常量
UPSTREAM_REPO = "ardeyouxipianyi/workbuddy2api-hub"
RELEASES_API = f"https://api.github.com/repos/{UPSTREAM_REPO}/releases"
USER_AGENT = "WorkBuddy-Desktop-Repacker/1.0"
REQUEST_TIMEOUT = 10

if getattr(sys, "frozen", False):
    PROJECT_DIR = os.path.dirname(sys.executable)
else:
    PROJECT_DIR = os.path.dirname(os.path.abspath(__file__))


# 全局升级任务状态
_LOCK = threading.Lock()
UPGRADE_JOB = {
    "status": "idle",       # "idle" | "running" | "success" | "error"
    "step": 0,              # 0..5
    "step_name": "就绪",
    "progress": 0,          # 0..100
    "logs": [],
    "error": None,
    "target_tag": "",
    "target_version": "",
    "new_exe_ready": False,
    "start_time": 0.0,
    "end_time": 0.0,
}


def _append_log(msg):
    with _LOCK:
        timestamp = time.strftime("%H:%M:%S")
        log_line = f"[{timestamp}] {msg}"
        UPGRADE_JOB["logs"].append(log_line)
        # 最多保留 2000 行日志
        if len(UPGRADE_JOB["logs"]) > 2000:
            UPGRADE_JOB["logs"] = UPGRADE_JOB["logs"][-2000:]
    try:
        sys.stdout.write(log_line + "\n")
        if hasattr(sys.stdout, "flush"):
            sys.stdout.flush()
    except Exception:
        pass


def _set_step(step, name, progress):
    with _LOCK:
        UPGRADE_JOB["step"] = step
        UPGRADE_JOB["step_name"] = name
        UPGRADE_JOB["progress"] = progress
    _append_log(f">>> 步骤 {step}/5: {name}")


def get_current_version():
    """获取当前正在运行的网关版本号，如 '1.6.18'。"""
    try:
        from . import wb_proxy
        if hasattr(wb_proxy, "running_version"):
            return wb_proxy.running_version()
    except Exception:
        pass
    # 备用：从 wb_proxy.py 文件中解析
    proxy_file = os.path.join(PROJECT_DIR, "wb_proxy.py")
    if os.path.isfile(proxy_file):
        try:
            with open(proxy_file, "r", encoding="utf-8", errors="replace") as f:
                content = f.read(300000)
                import re
                m = re.search(r'server_version\s*=\s*["\']wb-proxy/([^"\']+)["\']', content)
                if m:
                    return m.group(1).strip()
        except Exception:
            pass
    return "1.6.18"


def check_upstream_release(timeout=REQUEST_TIMEOUT):
    """查询 GitHub 上游最新 Release 信息，并与当前版本对比。"""
    current_ver = get_current_version()
    req = urllib.request.Request(RELEASES_API, headers={
        "User-Agent": USER_AGENT,
        "Accept": "application/vnd.github+json"
    })
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8", errors="replace"))
        if not isinstance(data, list) or not data:
            raise ValueError("Release 数据为空或格式异常")
    except Exception as exc:
        # 如果 API 失败（可能受 GitHub 限流），尝试通过 git ls-remote 获取 tags
        git_tags = _fallback_check_git_tags()
        if git_tags:
            latest_tag = git_tags[-1]
            latest_ver = latest_tag.lstrip("v")
            has_update = wb_updates.is_newer(latest_ver, current_ver)
            return {
                "ok": True,
                "current_version": current_ver,
                "latest_version": latest_ver,
                "tag": latest_tag,
                "published_at": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "release_url": f"https://github.com/{UPSTREAM_REPO}/releases/tag/{latest_tag}",
                "body": "由本地 Git 标签检索到最新发布版本",
                "has_update": has_update,
                "source": "git_remote"
            }
        return {
            "ok": False,
            "current_version": current_ver,
            "latest_version": None,
            "tag": None,
            "has_update": False,
            "error": f"检查上游发布失败: {type(exc).__name__} {exc}"
        }

    # 提取最新的非 draft、非 prerelease 版本
    picked = wb_updates.pick_latest(data, current_ver)
    if not picked:
        return {
            "ok": True,
            "current_version": current_ver,
            "latest_version": current_ver,
            "tag": "v" + current_ver,
            "has_update": False,
            "body": "暂无更新",
        }

    latest_ver = picked.get("version")
    latest_tag = picked.get("tag") or ("v" + latest_ver)
    has_update = picked.get("newer", False)

    # 提取对应 release 的正文说明
    body_text = ""
    for entry in data:
        if isinstance(entry, dict) and entry.get("tag_name") == latest_tag:
            body_text = entry.get("body") or ""
            break

    return {
        "ok": True,
        "current_version": current_ver,
        "latest_version": latest_ver,
        "tag": latest_tag,
        "published_at": picked.get("published_at"),
        "release_url": picked.get("url") or f"https://github.com/{UPSTREAM_REPO}/releases/tag/{latest_tag}",
        "body": body_text[:3000],
        "has_update": has_update,
        "source": "github_api"
    }


def _fallback_check_git_tags():
    """尝试用 git ls-remote 查看远端 tags。"""
    try:
        git_cmd = shutil.which("git") or "git"
        res = subprocess.run(
            [git_cmd, "ls-remote", "--tags", f"https://github.com/{UPSTREAM_REPO}.git"],
            cwd=PROJECT_DIR,
            capture_output=True,
            text=True,
            timeout=8
        )
        if res.returncode == 0:
            import re
            tags = []
            for line in res.stdout.splitlines():
                m = re.search(r"refs/tags/(v\d+\.\d+\.\d+)$", line.strip())
                if m:
                    tags.append(m.group(1))
            tags.sort(key=lambda t: wb_updates.parse_version(t) or (0, 0, 0))
            return tags
    except Exception:
        pass
    return []


def find_python_executable():
    """查找本机可用于执行 pip 和 pyinstaller 的 Python 解释器。"""
    # 1. py 启动器
    py_launcher = shutil.which("py")
    if py_launcher:
        return [py_launcher]

    # 2. python/python3
    py_path = shutil.which("python") or shutil.which("python3")
    if py_path:
        return [py_path]

    # 3. 常见 Windows 安装路径
    candidates = [
        os.path.expandvars(r"%LOCALAPPDATA%\Programs\Python\Python313\python.exe"),
        os.path.expandvars(r"%LOCALAPPDATA%\Programs\Python\Python312\python.exe"),
        os.path.expandvars(r"%LOCALAPPDATA%\Programs\Python\Python311\python.exe"),
        r"C:\Windows\py.exe",
        r"C:\Python313\python.exe",
        r"C:\Python312\python.exe",
    ]
    for c in candidates:
        if os.path.isfile(c):
            return [c]

    if not getattr(sys, "frozen", False):
        return [sys.executable]

    return ["py"]


def pull_upstream_code(target_tag="v1.6.19"):
    """拉取上游最新发布代码并合并到工作区。"""
    _append_log(f"开始拉取上游版本: {target_tag} ...")

    # 检查是否存在 temp_repo 或 .git
    temp_repo_dir = os.path.join(PROJECT_DIR, "temp_repo")
    root_git_dir = os.path.join(PROJECT_DIR, ".git")

    git_success = False
    git_cmd = shutil.which("git") or "git"

    # 优先使用 git pull / fetch
    if os.path.isdir(temp_repo_dir) and os.path.isdir(os.path.join(temp_repo_dir, ".git")):
        _append_log(f"在 {temp_repo_dir} 中执行 git fetch & checkout ...")
        try:
            r1 = subprocess.run([git_cmd, "-C", temp_repo_dir, "fetch", "origin", "--tags"],
                                capture_output=True, text=True, timeout=30)
            _append_log(f"git fetch 输出: {r1.stdout or r1.stderr or 'OK'}")
            r2 = subprocess.run([git_cmd, "-C", temp_repo_dir, "checkout", target_tag],
                                capture_output=True, text=True, timeout=15)
            _append_log(f"git checkout {target_tag} 输出: {r2.stdout or r2.stderr or 'OK'}")
            if r2.returncode == 0 or r1.returncode == 0:
                git_success = True
                _sync_files_from_dir(temp_repo_dir, PROJECT_DIR)
        except Exception as e:
            _append_log(f"temp_repo git 同步失败 ({e})，将尝试备用方式")

    if not git_success and os.path.isdir(root_git_dir):
        _append_log("在根目录中检测到 .git，尝试执行 git pull ...")
        try:
            r = subprocess.run([git_cmd, "fetch", "origin", "--tags"],
                               cwd=PROJECT_DIR, capture_output=True, text=True, timeout=30)
            _append_log(f"git fetch 输出: {r.stdout or r.stderr or 'OK'}")
            git_success = (r.returncode == 0)
        except Exception as e:
            _append_log(f"根目录 git fetch 失败: {e}")

    # 如果没有 git 或 git 失败，直接通过 GitHub 下载 release archive zip
    if not git_success:
        zip_url = f"https://codeload.github.com/{UPSTREAM_REPO}/zip/refs/tags/{target_tag}"
        _append_log(f"尝试直接从 GitHub 下载源码压缩包: {zip_url} ...")
        download_dir = os.path.join(PROJECT_DIR, "build", "upstream_zip")
        os.makedirs(download_dir, exist_ok=True)
        zip_file = os.path.join(download_dir, f"{target_tag}.zip")

        req = urllib.request.Request(zip_url, headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(req, timeout=60) as resp, open(zip_file, "wb") as f:
            shutil.copyfileobj(resp, f)
        _append_log(f"源码包下载完成 ({os.path.getsize(zip_file)} 字节)，正在解压缩...")

        extract_dir = os.path.join(download_dir, "extracted")
        if os.path.isdir(extract_dir):
            shutil.rmtree(extract_dir, ignore_errors=True)
        os.makedirs(extract_dir, exist_ok=True)

        with zipfile.ZipFile(zip_file, "r") as z:
            z.extractall(extract_dir)

        # zip 内通常会包含一层根目录，例如 workbuddy2api-hub-1.6.19
        subdirs = [os.path.join(extract_dir, d) for d in os.listdir(extract_dir) if os.path.isdir(os.path.join(extract_dir, d))]
        src_root = subdirs[0] if subdirs else extract_dir
        _sync_files_from_dir(src_root, PROJECT_DIR)

    _append_log("上游代码拉取并同步就绪！")


def _sync_files_from_dir(src_dir, dst_dir):
    """从源目录同步更新文件到目标目录，严格保护用户隐私配置与桌面端关键定制。"""
    # 绝对不能覆盖的用户数据和桌面定制文件
    PROTECTED_ITEMS = {
        "accounts",             # 用户账号凭据
        "usage",                # 用量记录
        "pricing",              # 定价策略
        "app.log",              # 运行日志
        "app_icon.ico",         # 图标
        "desktop_app.py",       # 桌面程序入口
        "WorkBuddy.spec",       # 打包规格
        "build_exe.bat",        # 构建脚本
        "WorkBuddy.exe",        # 运行中的主程序
        "build",                # 构建产物
        "dist",                 # 输出产物
        "temp_repo",            # 辅助镜像目录
        ".git",                 # git 目录
    }

    _append_log(f"正在从 {src_dir} 同步核心代码至 {dst_dir} ...")
    count = 0
    for root, dirs, files in os.walk(src_dir):
        # 排除受保护的子目录
        dirs[:] = [d for d in dirs if d not in PROTECTED_ITEMS and not d.startswith(".")]

        rel_dir = os.path.relpath(root, src_dir)
        target_dir = dst_dir if rel_dir == "." else os.path.join(dst_dir, rel_dir)
        os.makedirs(target_dir, exist_ok=True)

        for f in files:
            if f in PROTECTED_ITEMS or f.endswith(".pyc") or f.endswith(".log"):
                continue
            # 特殊处理 dashboard.html：由专门的合并逻辑处理，避免直接覆盖掉现代美化与桌面端升级接口
            if f == "dashboard.html":
                continue

            src_file = os.path.join(root, f)
            dst_file = os.path.join(target_dir, f)
            try:
                shutil.copy2(src_file, dst_file)
                count += 1
            except Exception as e:
                _append_log(f"复制文件 {f} 失败: {e}")

    _append_log(f"已同步 {count} 个代码与资源文件。")


def apply_desktop_patches():
    """检查并应用桌面端所需补丁（wb_proxy.py 中的环境兼容与更新接口注册）。"""
    _append_log("正在校验并应用桌面端补丁...")
    proxy_path = os.path.join(PROJECT_DIR, "wb_proxy.py")
    if not os.path.isfile(proxy_path):
        raise FileNotFoundError(f"未找到核心文件 {proxy_path}")

    with open(proxy_path, "r", encoding="utf-8", errors="replace") as f:
        code = f.read()

    modified = False

    # 1. 注入 DASHBOARD_HTML 环境变量支持
    target_dash = 'DASHBOARD_HTML = os.environ.get("WB_DASHBOARD_HTML") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "dashboard.html")'
    if 'DASHBOARD_HTML = os.environ.get("WB_DASHBOARD_HTML")' not in code:
        code = code.replace(
            'DASHBOARD_HTML = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dashboard.html")',
            target_dash
        )
        modified = True

    # 2. 注入 ACCOUNTS_DIR 环境变量支持
    target_acct = 'ACCOUNTS_DIR = os.environ.get("WB_ACCOUNTS_DIR") or os.environ.get("ACCOUNTS_DIR") or os.path.join(os.path.dirname(os.path.abspath(__file__)), \'accounts\')'
    if 'ACCOUNTS_DIR = os.environ.get("WB_ACCOUNTS_DIR")' not in code:
        code = code.replace(
            "ACCOUNTS_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'accounts')",
            target_acct
        )
        modified = True

    # 3. 注入 HTTP_SERVER 实例引用（方便优雅重启与关闭）
    if 'global HTTP_SERVER\n        HTTP_SERVER = server' not in code and 'HTTP_SERVER = server' not in code:
        code = code.replace(
            'server = ThreadingHTTPServer((args.host, args.port), Handler)',
            'server = ThreadingHTTPServer((args.host, args.port), Handler)\n        global HTTP_SERVER\n        HTTP_SERVER = server'
        )
        modified = True

    # 4. 注入在线更新与重新打包路由处理
    if 'self._route_repack' not in code:
        # 在 Handler 的 do_GET 中增加路由分支
        get_marker = 'if path == "/updates":\n            return self._get_updates()'
        get_replacement = """if path == "/updates":
            return self._get_updates()
        if path == "/updates/release-info":
            return self._route_updates_release_info()
        if path == "/updates/repack-progress":
            return self._route_updates_repack_progress()"""
        if get_marker in code:
            code = code.replace(get_marker, get_replacement)
            modified = True

        # 在 Handler 的 do_POST 中增加路由分支
        post_marker = 'if path == "/updates/check":\n            return self._route_update_check()'
        post_replacement = """if path == "/updates/check":
            return self._route_update_check()
        if path == "/updates/repack":
            return self._route_updates_repack(payload)
        if path == "/updates/restart":
            return self._route_updates_restart()"""
        if post_marker in code:
            code = code.replace(post_marker, post_replacement)
            modified = True

        # 在 Handler 类中添加对应的路由处理方法
        method_marker = "    def _route_update_check(self):"
        route_methods = """    def _route_updates_release_info(self):
        if not self._authorized():
            return
        from . import wb_repack
        info = wb_repack.check_upstream_release()
        with wb_repack._LOCK:
            info["job"] = dict(wb_repack.UPGRADE_JOB)
        return self._json(200, info)

    def _route_updates_repack_progress(self):
        if not self._authorized():
            return
        from . import wb_repack
        with wb_repack._LOCK:
            job = dict(wb_repack.UPGRADE_JOB)
        return self._json(200, {"ok": True, "job": job})

    def _route_updates_repack(self, payload):
        if not self._authorized():
            return
        from . import wb_repack
        auto_restart = bool(payload.get("auto_restart", False))
        force = bool(payload.get("force", False))
        target_tag = payload.get("target_tag")
        res = wb_repack.start_upgrade_job(target_tag=target_tag, auto_restart=auto_restart, force=force)
        return self._json(200, res)

    def _route_updates_restart(self):
        if not self._authorized():
            return
        from . import wb_repack
        wb_repack.restart_exe()
        return self._json(200, {"ok": True, "msg": "正在重启应用..."})

"""
        if method_marker in code:
            code = code.replace(method_marker, route_methods + method_marker)
            modified = True

    if modified:
        with open(proxy_path, "w", encoding="utf-8") as f:
            f.write(code)
        _append_log("wb_proxy.py 桌面端适配补丁已更新写入。")
    else:
        _append_log("wb_proxy.py 适配补丁完整，无需额外修改。")


def repack_exe():
    """调用 PyInstaller 进行桌面程序重新打包。"""
    _append_log("开始执行 PyInstaller 重新打包流程...")

    py_cmd = find_python_executable()
    _append_log(f"使用的 Python 启动器: {' '.join(py_cmd)}")

    # 验证 pyinstaller 是否可用
    try:
        check_p = subprocess.run(py_cmd + ["-m", "PyInstaller", "--version"],
                                 cwd=PROJECT_DIR, capture_output=True, text=True, timeout=10)
        _append_log(f"PyInstaller 版本: {check_p.stdout.strip()}")
    except Exception as e:
        raise RuntimeError(f"PyInstaller 检查失败，请确认是否已安装: {e}")

    # 规范打包参数
    cmd = py_cmd + [
        "-m", "PyInstaller",
        "--noconsole",
        "--onefile",
        "--name", "WorkBuddy",
        "--icon", "app_icon.ico",
        "--add-data", "dashboard.html;.",
        "--add-data", "app_icon.ico;.",
        "--add-data", "pricing;pricing",
        "--collect-all", "webview",
        "--collect-all", "pythonnet",
        "--collect-all", "clr_loader",
        "desktop_app.py"
    ]

    _append_log(f"打包命令: {' '.join(cmd)}")
    _append_log("正在编译资源与依赖项，请耐心等待（通常耗时约 30-50 秒）...")

    proc = subprocess.Popen(
        cmd,
        cwd=PROJECT_DIR,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1
    )

    for line in iter(proc.stdout.readline, ""):
        line_str = line.strip()
        if line_str:
            _append_log(f"[PyInstaller] {line_str}")

    proc.stdout.close()
    return_code = proc.wait()

    if return_code != 0:
        raise RuntimeError(f"PyInstaller 打包失败，退出码: {return_code}")

    dist_exe = os.path.join(PROJECT_DIR, "dist", "WorkBuddy.exe")
    if not os.path.isfile(dist_exe):
        raise FileNotFoundError(f"未找到生成的产物 {dist_exe}")

    exe_size_mb = os.path.getsize(dist_exe) / (1024 * 1024)
    _append_log(f"打包完成！生成产物: {dist_exe} (大小: {exe_size_mb:.2f} MB)")
    return dist_exe


def replace_running_exe(dist_exe):
    """在 Windows 上安全替换正在运行的 WorkBuddy.exe。"""
    target_exe = os.path.join(PROJECT_DIR, "WorkBuddy.exe")
    _append_log(f"正在更新可执行文件: {target_exe} ...")

    old_exe = os.path.join(PROJECT_DIR, "WorkBuddy.exe.old")
    if os.path.isfile(old_exe):
        try:
            os.remove(old_exe)
        except Exception:
            pass

    if os.path.isfile(target_exe):
        # 利用 Windows NTFS 特性：运行中的 exe 允许重命名，但不允许直接覆写
        try:
            os.rename(target_exe, old_exe)
            _append_log("已将原有运行中的 WorkBuddy.exe 重命名为 WorkBuddy.exe.old")
        except Exception as e:
            _append_log(f"重命名原有程序警告: {e}，尝试直接拷贝")

    shutil.copy2(dist_exe, target_exe)
    _append_log(f"新版 WorkBuddy.exe 已成功部署至根目录！")


def restart_exe():
    """启动新生成的 WorkBuddy.exe 并退出当前实例。"""
    target_exe = os.path.join(PROJECT_DIR, "WorkBuddy.exe")
    _append_log("准备重启应用程序...")

    if not os.path.isfile(target_exe):
        # 调试开发模式下可能直接启动 python desktop_app.py
        py_cmd = find_python_executable()
        cmd = py_cmd + [os.path.join(PROJECT_DIR, "desktop_app.py")]
    else:
        cmd = [target_exe]

    try:
        # 分离进程启动新实例
        DETACHED_FLAGS = 0x00000008 | 0x00000200  # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        subprocess.Popen(cmd, cwd=PROJECT_DIR, creationflags=DETACHED_FLAGS)
        _append_log("已成功唤起新进程，即将关闭当前实例。")
    except Exception as e:
        _append_log(f"唤起新进程失败: {e}")
        return False

    def _shutdown_later():
        time.sleep(0.8)
        try:
            import desktop_app
            if desktop_app._CURRENT_WINDOW:
                desktop_app._CURRENT_WINDOW.destroy()
        except Exception:
            pass
        os._exit(0)

    threading.Thread(target=_shutdown_later, daemon=True).start()
    return True


def _run_upgrade_workflow(target_tag, auto_restart):
    """后台升级工作流执行线程。"""
    global UPGRADE_JOB
    with _LOCK:
        UPGRADE_JOB["status"] = "running"
        UPGRADE_JOB["start_time"] = time.time()
        UPGRADE_JOB["error"] = None
        UPGRADE_JOB["logs"].clear()
        UPGRADE_JOB["new_exe_ready"] = False

    try:
        _set_step(1, "检测上游版本与打包环境", 15)
        info = check_upstream_release()
        resolved_tag = target_tag or info.get("tag") or ("v" + get_current_version())
        with _LOCK:
            UPGRADE_JOB["target_tag"] = resolved_tag
            UPGRADE_JOB["target_version"] = resolved_tag.lstrip("v")
        _append_log(f"目标升级版本: {resolved_tag}")

        _set_step(2, "拉取并同步上游核心源码", 35)
        pull_upstream_code(target_tag=resolved_tag)

        _set_step(3, "校验补丁与桌面环境兼容性", 50)
        apply_desktop_patches()

        _set_step(4, "调用 PyInstaller 进行完整打包", 75)
        dist_exe = repack_exe()

        _set_step(5, "热替换可执行文件与收尾", 95)
        replace_running_exe(dist_exe)

        with _LOCK:
            UPGRADE_JOB["status"] = "success"
            UPGRADE_JOB["step"] = 5
            UPGRADE_JOB["step_name"] = "升级与打包成功！"
            UPGRADE_JOB["progress"] = 100
            UPGRADE_JOB["new_exe_ready"] = True
            UPGRADE_JOB["end_time"] = time.time()

        _append_log(f"=== 全部流程顺利完成！WorkBuddy.exe 已就绪 ===")

        if auto_restart:
            _append_log("用户已开启自动重启，正在重启应用...")
            time.sleep(1.2)
            restart_exe()

    except Exception as exc:
        err_msg = str(exc)
        _append_log(f"[严重错误] 升级/打包过程终止: {err_msg}")
        with _LOCK:
            UPGRADE_JOB["status"] = "error"
            UPGRADE_JOB["error"] = err_msg
            UPGRADE_JOB["end_time"] = time.time()


def start_upgrade_job(target_tag=None, auto_restart=False, force=False):
    """启动后台升级与打包任务。"""
    with _LOCK:
        if UPGRADE_JOB["status"] == "running":
            return {"ok": False, "msg": "当前已有打包任务正在运行中，请勿重复触发。"}

    th = threading.Thread(
        target=_run_upgrade_workflow,
        args=(target_tag, auto_restart),
        daemon=True,
        name="wb-repack-worker"
    )
    th.start()
    return {"ok": True, "msg": "升级与打包任务已启动，正在后台进行处理。"}
