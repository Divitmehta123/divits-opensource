@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-binary.ps1" -SourceDirectory "%~dp0"
if errorlevel 1 exit /b %errorlevel%
endlocal
