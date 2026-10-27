@echo off
rem Creates the .venv and installs WinPods' dependencies (needs uv or Python 3.11+).
cd /d "%~dp0"
where uv >nul 2>nul
if %errorlevel%==0 (
    uv venv .venv --python 3.13
    uv pip install --python .venv\Scripts\python.exe -r requirements.txt
) else (
    python -m venv .venv
    .venv\Scripts\python.exe -m pip install -r requirements.txt
)
echo.
echo Done. Start WinPods with run.bat (or double-click WinPods.pyw).
