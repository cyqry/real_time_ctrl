@echo off
setlocal
cd /d "%~dp0"

"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\publish_stack.ps1" -Channel Gray %*
set "RTC_EXIT=%ERRORLEVEL%"
echo.
if "%RTC_EXIT%"=="0" (
    echo Gray workflow completed. See the plan and report for performed actions.
) else (
    echo Gray publication failed with exit code %RTC_EXIT%.
)
pause
exit /b %RTC_EXIT%
