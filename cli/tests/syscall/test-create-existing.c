#define _GNU_SOURCE
#include "test-common.h"
#include <fcntl.h>
#include <unistd.h>

/*
 * Regression test for create_file() failing with EEXIST on existing files.
 *
 * The FUSE create op is called by the kernel for open(O_CREAT) on existing
 * files (e.g. cargo overwriting .d dependency files). create_file must
 * truncate rather than fail with EEXIST.
 */
int test_create_existing(const char *base_path) {
    char path[512];
    char buf[256];
    int fd;
    ssize_t n;

    snprintf(path, sizeof(path), "%s/create_existing_test.txt", base_path);

    /* Step 1: Create a file with some content */
    fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "initial create should succeed");
    n = write(fd, "old content", 11);
    TEST_ASSERT_ERRNO(n == 11, "initial write should succeed");
    close(fd);

    /* Step 2: Open with O_CREAT again on the existing file — must not fail */
    fd = open(path, O_RDWR | O_CREAT, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "open O_CREAT on existing file should succeed (not EEXIST)");

    /* Step 3: Verify the file is accessible (write + read back) */
    n = pwrite(fd, "new", 3, 0);
    TEST_ASSERT_ERRNO(n == 3, "write to re-created file should succeed");

    n = pread(fd, buf, sizeof(buf), 0);
    TEST_ASSERT_ERRNO(n >= 3, "read from re-created file should succeed");
    TEST_ASSERT(memcmp(buf, "new", 3) == 0, "re-created file should contain new data");
    close(fd);

    /* Step 4: O_CREAT | O_TRUNC on existing file must truncate */
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "open O_CREAT|O_TRUNC on existing file should succeed");

    n = pread(fd, buf, sizeof(buf), 0);
    TEST_ASSERT(n == 0, "truncated file should be empty");

    n = pwrite(fd, "truncated", 9, 0);
    TEST_ASSERT_ERRNO(n == 9, "write after truncate should succeed");

    n = pread(fd, buf, sizeof(buf), 0);
    TEST_ASSERT_ERRNO(n == 9, "read after write should return 9 bytes");
    buf[n] = '\0';
    TEST_ASSERT(strcmp(buf, "truncated") == 0, "content should match what was written");
    close(fd);

    return 0;
}
