#include "test-common.h"
#include <stdio.h>
#include <string.h>

typedef struct {
    const char *name;
    int (*test_func)(const char *);
} test_case_t;

int main(int argc, char *argv[]) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s <base_path>\n", argv[0]);
        fprintf(stderr, "Example: %s /sandbox\n", argv[0]);
        return 1;
    }

    const char *base_path = argv[1];

    /* Define all test cases */
    test_case_t tests[] = {
        {"openat", test_openat},
        {"read", test_read},
        {"write", test_write},
        {"close", test_close},
        {"dup", test_dup},
        {"dup2", test_dup2},
        {"stat", test_stat},
        {"fstat", test_fstat},
        {"lstat", test_lstat},
        {"getdents64", test_getdents64},
        {"append_existing", test_append_existing},
        {"pwrite_nested", test_pwrite_nested},
        {"pread_sparse", test_pread_sparse},
        {"link", test_link},
        {"unlink", test_unlink},
        {"copyup_inode_stability", test_copyup_inode_stability},
        {"rename", test_rename},
        {"mknod", test_mknod},
        {"mkfifo", test_mkfifo},
    };

    int num_tests = sizeof(tests) / sizeof(tests[0]);
    int passed = 0;
    int failed = 0;

    printf("Running syscall tests with base_path: %s\n", base_path);
    printf("===========================================\n\n");

    /* Run all tests */
    for (int i = 0; i < num_tests; i++) {
        printf("Running test: %s\n", tests[i].name);

        int result = tests[i].test_func(base_path);

        if (result == 0) {
            printf(COLOR_GREEN "PASS: %s" COLOR_RESET "\n\n", tests[i].name);
            passed++;
        } else {
            printf(COLOR_RED "FAIL: %s" COLOR_RESET "\n\n", tests[i].name);
            failed++;
        }
    }

    /* Print summary */
    printf("===========================================\n");
    printf("Test Summary:\n");
    printf("  Total:  %d\n", num_tests);
    printf("  " COLOR_GREEN "Passed: %d" COLOR_RESET "\n", passed);
    if (failed > 0) {
        printf("  " COLOR_RED "Failed: %d" COLOR_RESET "\n", failed);
    } else {
        printf("  Failed: %d\n", failed);
    }
    printf("===========================================\n");

    if (failed == 0) {
        printf(COLOR_GREEN "All tests passed!" COLOR_RESET "\n");
    }

    return (failed == 0) ? 0 : 1;
}
