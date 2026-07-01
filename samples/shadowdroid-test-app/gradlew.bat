@echo off
setlocal
set ROOT=%~dp0
call "%ROOT%..\..\server\gradlew.bat" -p "%ROOT%" %*

