#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

int test_mkfifo(const char *base_path) {
    char path[512];
    struct stat st;
    int result;
    mode_t old_umask;

    /* Save and clear umask for predictable permission tests */
    old_umask = umask(0);

    /* Test 1: Create a FIFO (named pipe) using mkfifo */
    snprintf(path, sizeof(path), "%s/test_fifo_mkfifo", base_path);
    unlink(path);  /* Clean up any previous test file */

    result = mkfifo(path, 0644);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo creation should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT(S_ISFIFO(st.st_mode), "created node should be a FIFO");
    TEST_ASSERT((st.st_mode & 0777) == 0644, "FIFO should have correct permissions (0644)");

    unlink(path);

    /* Test 2: mkfifo with existing path should fail with EEXIST */
    snprintf(path, sizeof(path), "%s/test.txt", base_path);

    result = mkfifo(path, 0644);
    TEST_ASSERT(result < 0, "mkfifo on existing file should fail");
    TEST_ASSERT(errno == EEXIST, "errno should be EEXIST for existing path");

    /* Test 3: mkfifo in non-existent directory should fail with ENOENT */
    snprintf(path, sizeof(path), "%s/nonexistent_dir/test_mkfifo", base_path);

    result = mkfifo(path, 0644);
    TEST_ASSERT(result < 0, "mkfifo in non-existent directory should fail");
    TEST_ASSERT(errno == ENOENT, "errno should be ENOENT for non-existent directory");

    /* Test 4: Create a FIFO with different permissions (0755) */
    snprintf(path, sizeof(path), "%s/test_fifo_perms_755", base_path);
    unlink(path);

    result = mkfifo(path, 0755);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo with 0755 should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT((st.st_mode & 0777) == 0755, "FIFO should have 0755 permissions");

    unlink(path);

    /* Test 5: Create a FIFO with restrictive permissions (0600) */
    snprintf(path, sizeof(path), "%s/test_fifo_perms_600", base_path);
    unlink(path);

    result = mkfifo(path, 0600);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo with 0600 should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT((st.st_mode & 0777) == 0600, "FIFO should have 0600 permissions");

    unlink(path);

    /* Test 6: Create FIFO in subdirectory */
    char subdir_path[512];
    snprintf(subdir_path, sizeof(subdir_path), "%s/mkfifo_subdir", base_path);
    mkdir(subdir_path, 0755);

    snprintf(path, sizeof(path), "%s/mkfifo_subdir/test_fifo", base_path);
    unlink(path);

    result = mkfifo(path, 0644);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo in subdirectory should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO in subdirectory should succeed");
    TEST_ASSERT(S_ISFIFO(st.st_mode), "created node in subdir should be a FIFO");

    unlink(path);
    rmdir(subdir_path);

    /* Test 7: Create FIFO and verify it has size 0 */
    snprintf(path, sizeof(path), "%s/test_fifo_size", base_path);
    unlink(path);

    result = mkfifo(path, 0644);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT(st.st_size == 0, "FIFO should have size 0");

    unlink(path);

    /* Test 8: Create FIFO and verify link count is 1 */
    snprintf(path, sizeof(path), "%s/test_fifo_nlink", base_path);
    unlink(path);

    result = mkfifo(path, 0644);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT(st.st_nlink == 1, "FIFO should have nlink == 1");

    unlink(path);

    /* Test 9: Create FIFO with world-writable permissions (0666) */
    snprintf(path, sizeof(path), "%s/test_fifo_666", base_path);
    unlink(path);

    result = mkfifo(path, 0666);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo with 0666 should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT((st.st_mode & 0777) == 0666, "FIFO should have 0666 permissions");

    unlink(path);

    /* Test 10: Verify FIFO shows up in directory listing */
    snprintf(path, sizeof(path), "%s/test_fifo_readdir", base_path);
    unlink(path);

    result = mkfifo(path, 0644);
    TEST_ASSERT_ERRNO(result == 0, "mkfifo should succeed");

    /* Verify file exists via lstat too */
    result = lstat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "lstat on FIFO should succeed");
    TEST_ASSERT(S_ISFIFO(st.st_mode), "lstat should report FIFO type");

    unlink(path);

    /* Restore original umask */
    umask(old_umask);

    return 0;
}
