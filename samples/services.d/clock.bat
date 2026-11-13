@echo off
rem Example svcd core service.
rem The daemon invokes this same file with `start`, `stop` or `status`.
rem Copy it into <config>\services.d\clock.bat.
setlocal
set "ACTION=%~1"
if "%ACTION%"=="" set "ACTION=%SVCD_ACTION%"
if "%ACTION%"=="" set "ACTION=start"
set "STOP_FILE=%~dp0%~n0.stop"

if /I "%ACTION%"=="start" goto start
if /I "%ACTION%"=="stop" goto stop
if /I "%ACTION%"=="status" goto status
exit /b 64

:start
del "%STOP_FILE%" >nul 2>&1
:loop
if exist "%STOP_FILE%" goto cleanup
echo %DATE% %TIME%
rem ping -n 3 waits about two seconds without needing a console for `timeout`.
ping -n 3 127.0.0.1 >nul 2>&1
goto loop

:stop
> "%STOP_FILE%" echo stop
exit /b 0

:status
exit /b 0

:cleanup
del "%STOP_FILE%" >nul 2>&1
exit /b 0
