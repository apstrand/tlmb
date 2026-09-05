#ifndef MAILBOX_H
#define MAILBOX_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct MailboxHandle MailboxHandle;

/* Opens a mailbox rooted at `config_path` / `state_dir`. On failure returns
 * NULL and, if `error_out` is non-NULL, a heap string the caller frees with
 * mailbox_string_free. */
MailboxHandle *mailbox_open(const char *config_path, const char *state_dir,
                            char **error_out);

void mailbox_close(MailboxHandle *handle);

/* Runs one mailbox-api command. `method` is a command name (`ls`, `show`,
 * `sync`, …); `args_json` is the argument object, or "{}" / NULL. The result
 * is a heap JSON document — a mailbox/v1 Reply on success, or
 * {"error":{...}} on failure — which the caller frees with
 * mailbox_string_free. */
char *mailbox_exec(MailboxHandle *handle, const char *method,
                   const char *args_json);

void mailbox_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif
