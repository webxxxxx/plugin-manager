@echo off
rem Example svcd service: a core drop-in script and a schema-2 plugin payload.
rem svcd calls only the actions this file declares: start, stop and status.
rem `stop` runs as a separate process before the service tree is terminated;
rem its stderr goes to the service log (stop stdout is discarded).
rem `status` stdout is shown as supplementary detail by `svc status` / `plugin status`.
rem Copy it into <config>\services.d\clock.bat, or bundle it with:
rem   plugin bundle samples\clock --from-core --interpreter cmd --interpreter-arg /C --key samples\keys\sample-2026.key
setlocal
set "ACTION=%~1"

if /I "%ACTION%"=="start" goto start
if /I "%ACTION%"=="stop" goto stop
if /I "%ACTION%"=="status" goto status
exit /b 64

:start
:loop
echo %DATE% %TIME%
rem ping -n 3 waits about two seconds without needing a console for `timeout`.
ping -n 3 127.0.0.1 >nul 2>&1
goto loop

:stop
echo invoke stop 1>&2
exit /b 0

:status
echo clock: printing the time every two seconds
exit /b 0
