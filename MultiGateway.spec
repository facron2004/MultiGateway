# -*- mode: python ; coding: utf-8 -*-
from PyInstaller.utils.hooks import collect_all, collect_submodules

datas = [
    ('gateway/vendor/workbuddy/dashboard.html', 'gateway/vendor/workbuddy'),
    ('gateway/vendor/workbuddy/wb_agents.py', 'gateway/vendor/workbuddy'),
    ('app_icon.ico', '.'), ('pricing', 'pricing'),
    ('gateway/dashboard.html', 'gateway'), ('gateway/ui', 'gateway/ui'), ('gateway/licenses', 'gateway/licenses'),
    ('LICENSE', 'gateway/licenses'),
    ('gateway/SOURCES.md', 'gateway'),
    ('gateway/vendor/trae/LICENSE', 'gateway/vendor/trae'),
    ('gateway/vendor/trae/web_login.py', 'gateway/vendor/trae'),
    ('gateway/vendor/qoder/LICENSE', 'gateway/vendor/qoder'),
    ('gateway/vendor/qoder/dashboard.html', 'gateway/vendor/qoder'),
    ('gateway/vendor/qoder/baseprompt.json', 'gateway/vendor/qoder'),
    ('gateway/vendor/qoder/qoder_catalog_cn.json', 'gateway/vendor/qoder'),
    ('gateway/vendor/qoder/qoder_catalog_intl.json', 'gateway/vendor/qoder'),
]
binaries, hiddenimports = [], collect_submodules('uvicorn')
for package in ('webview', 'pythonnet', 'clr_loader'):
    package_data, package_binaries, package_imports = collect_all(package)
    datas += package_data
    binaries += package_binaries
    hiddenimports += package_imports

a = Analysis(['gateway_app.py'], pathex=[], binaries=binaries, datas=datas,
             hiddenimports=hiddenimports, hookspath=[], hooksconfig={},
             runtime_hooks=[], excludes=['pytest', 'playwright', 'tkinter'],
             noarchive=False, optimize=0)
pyz = PYZ(a.pure)
exe = EXE(pyz, a.scripts, a.binaries, a.datas, [], name='MultiGateway',
          debug=False, bootloader_ignore_signals=False, strip=False, upx=True,
          console=False, disable_windowed_traceback=False, icon=['app_icon.ico'])
