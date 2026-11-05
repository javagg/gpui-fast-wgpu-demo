@echo off
rem Double-click this on the desktop. It opens the demo a few times, about a
rem minute in all, and saves screenshots and measurements to test-output.
rem Please leave the mouse out of the demo window while it runs.
setlocal
set "root=%~dp0.."
set "exe=%root%\target\release\cad_viewport.exe"
set "out=%root%\test-output"
if not exist "%out%" mkdir "%out%"

call :scenario interact "--exit-after 16"
call :scenario layout "--exit-after 8"
call :scenario idle "--exit-after 18"
call :scenario stress_10 "--array 10 --spin --exit-after 10"
call :scenario stress_30 "--array 30 --spin --exit-after 14"

echo.
echo Done. Results are in %out%
pause
exit /b

:scenario
echo Running %~1 ...
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0drive.ps1" -Exe "%exe%" -OutDir "%out%" -Scenario %~1 -AppArgs "%~2" > "%out%\%~1.log" 2>&1
type "%out%\%~1.log"
exit /b
