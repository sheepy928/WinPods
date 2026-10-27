@echo off
rem Builds a standalone dist\WinPods\WinPods.exe with PyInstaller.
cd /d "%~dp0"
where uv >nul 2>nul
if %errorlevel%==0 (
    uv pip install --python .venv\Scripts\python.exe pyinstaller
) else (
    .venv\Scripts\python.exe -m pip install pyinstaller
)
.venv\Scripts\python.exe -m PyInstaller --noconfirm --clean --windowed --name WinPods ^
    --collect-submodules pyatv --collect-data pyatv ^
    --hidden-import pystray._win32 ^
    WinPods.pyw
echo.
echo Built dist\WinPods\WinPods.exe
