@echo off
rem run_analyze_diff.cmd - build the C harness and run the diff vs Python.
call "%~dp0msys2_root.cmd"
if not "%MSYS2_ROOT%"=="" goto :have_msys
echo [ERROR] MSYS2_ROOT is not set and no default MSYS2 install was found.
echo Set MSYS2_ROOT to your MSYS2 root, then re-run run_analyze_diff.cmd.
echo See tools\msys2_root.cmd for the resolution rules.
exit /b 1
:have_msys
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
