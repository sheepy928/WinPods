@echo off
rem Starts WinPods (window + tray icon) without a console.
cd /d "%~dp0"
start "" ".venv\Scripts\pythonw.exe" "%~dp0WinPods.pyw"
