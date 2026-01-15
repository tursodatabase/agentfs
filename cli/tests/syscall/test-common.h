#ifndef TEST_COMMON_H
#define TEST_COMMON_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>

/* Test result tracking */
typedef struct {
    int passed;
    int failed;
} test_results_t;

/* Color codes for output */
#define COLOR_GREEN "\033[0;32m"
#define COLOR_RED "\033[0;31m"
#define COLOR_RESET "\033[0m"

/* Test assertion macros */
#define TEST_ASSERT(condition, message) do { \
    if (!(condition)) { \
        fprintf(stderr, COLOR_RED "FAIL: %s" COLOR_RESET "\n", message); \
        fprintf(stderr, "  at %s:%d\n", __FILE__, __LINE__); \
        return -1; \
    } \
} while (0)

#define TEST_ASSERT_ERRNO(condition, message) do { \
    if (!(condition)) { \
        fprintf(stderr, COLOR_RED "FAIL: %s" COLOR_RESET "\n", message); \
        fprintf(stderr, "  errno=%d (%s) at %s:%d\n", errno, strerror(errno), __FILE__, __LINE__); \
        return -1; \
    } \
} while (0)

/* Test function declarations */
int test_openat(const char *base_path);
int test_read(const char *base_path);
int test_write(const char *base_path);
int test_close(const char *base_path);
int test_dup(const char *base_path);
int test_dup2(const char *base_path);
int test_stat(const char *base_path);
int test_fstat(const char *base_path);
int test_lstat(const char *base_path);
int test_getdents64(const char *base_path);
int test_append_existing(const char *base_path);
int test_pwrite_nested(const char *base_path);
int test_pread_sparse(const char *base_path);
int test_link(const char *base_path);
int test_unlink(const char *base_path);
int test_copyup_inode_stability(const char *base_path);
int test_rename(const char *base_path);
int test_chown(const char *base_path);

#endif /* TEST_COMMON_H */
