/* Compiled by the build script through the admitted CC with -flto: the C compiler proper,
 * the assembler, collect2, lto-wrapper and the linker all run (shadow-backend profile). */
int shadow_backend_value(void) { return 42; }

int main(void) { return shadow_backend_value() - 42; }
