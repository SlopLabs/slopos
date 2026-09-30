# Stands in for CMake's FindPython3 where there is no interpreter. The LLVM
# runtimes' configure requires one, and only developer targets the C++
# runtime's build never names run it; an interpreter that refuses to run keeps
# any that ever does from passing.
set(Python3_EXECUTABLE "/bin/false")
set(Python3_Interpreter_FOUND TRUE)
set(Python3_FOUND TRUE)
