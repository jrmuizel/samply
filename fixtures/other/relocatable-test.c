// Test file for relocatable object file support
// This file contains DWARF 5 debug information that requires relocations

int add(int a, int b) {
    return a + b;
}

int multiply(int x, int y) {
    return x * y;
}

int main() {
    int result = add(5, 3);
    result = multiply(result, 2);
    return result;
}
