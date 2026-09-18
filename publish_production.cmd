@echo off
setlocal
cd /d "%~dp0"
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\publish_stack.ps1" -Channel Production %*
set "RTC_EXIT=%ERRORLEVEL%"
pause
exit /b %RTC_EXIT%
