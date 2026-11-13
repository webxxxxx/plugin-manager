@echo off
rem Example svcd core service.
rem svcd calls only the actions this file declares: `start` and `status`.
rem There is no `stop` action, so svcd stops the service by terminating the
rem process; `status` stdout is shown as supplementary detail by `svc status`.
rem Copy it into <config>\services.d\clock.bat.
setlocal
set "ACTION=%~1"

if /I "%ACTION%"=="start" goto start
if /I "%ACTION%"=="status" goto status
exit /b 64

:start
:loop
echo %DATE% %TIME%
rem ping -n 3 waits about two seconds without needing a console for `timeout`.
ping -n 3 127.0.0.1 >nul 2>&1
goto loop

:status
echo clock: printing the time every two seconds
exit /b 0
