@echo off
setlocal
cd /d "%~dp0"

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\publish_stack.ps1" -Channel Gray
set "RTC_EXIT=%ERRORLEVEL%"
echo.
if "%RTC_EXIT%"=="0" (
    echo Gray build, deployment and public acceptance tests completed successfully.
) else (
    echo Gray publication failed with exit code %RTC_EXIT%.
)
pause
exit /b %RTC_EXIT%
