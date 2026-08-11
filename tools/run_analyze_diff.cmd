@echo off
rem run_analyze_diff.cmd - build the C harness and run the diff vs Python.
set MSYS2_ROOT=C:\Users\Enis Polat\scoop\apps\msys2\current
set TOOLS=%~dp0
set ROOT=%TOOLS%..
set BUILD=%ROOT%\build
set SRC=%ROOT%\src

set PATH=%MSYS2_ROOT%\mingw64\bin;%PATH%
"%MSYS2_ROOT%\mingw64\bin\gcc.exe" -O2 -Wall -Wextra "%TOOLS%analyze_test.c" "%SRC%\analyze.c" -o "%BUILD%\analyze_test.exe"
if errorlevel 1 (
  echo BUILD FAILED
  exit /b 1
)
echo [OK] build\analyze_test.exe
python "%TOOLS%analyze_diff.py" %*
