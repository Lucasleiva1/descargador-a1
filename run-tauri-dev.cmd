@echo off
title Descargador A1 - Dev
cd /d "%~dp0"

echo Descargador A1 - servidor de desarrollo
echo Carpeta: %CD%
echo.
echo Iniciando npm run tauri:dev...
echo.

"C:\Program Files\nodejs\npm.cmd" run tauri:dev

echo.
echo El proceso termino con codigo %ERRORLEVEL%.
pause
