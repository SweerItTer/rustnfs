/* Small external Win32 driver for RustNFS GUI acceptance checks.
 * Build: x86_64-w64-mingw32-gcc -municode -Os windows_gui_driver.c -o windows_gui_driver.exe -luser32
 * Use only in an isolated test desktop/prefix containing one RustNFS window.
 */
#ifndef UNICODE
#define UNICODE
#endif
#define _UNICODE
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>

static void json_string(const wchar_t *value) {
    int len = WideCharToMultiByte(CP_UTF8, 0, value, -1, NULL, 0, NULL, NULL);
    char *bytes = calloc((size_t)len + 1, 1);
    WideCharToMultiByte(CP_UTF8, 0, value, -1, bytes, len, NULL, NULL);
    putchar('"');
    for (const unsigned char *p = (unsigned char *)bytes; *p; p++) {
        if (*p == '"' || *p == '\\') { putchar('\\'); putchar(*p); }
        else if (*p == '\n') fputs("\\n", stdout);
        else if (*p == '\r') fputs("\\r", stdout);
        else if (*p == '\t') fputs("\\t", stdout);
        else if (*p < 32) printf("\\u%04x", *p);
        else putchar(*p);
    }
    putchar('"');
    free(bytes);
}

static void text(HWND hwnd, wchar_t *out, int size) {
    DWORD_PTR result = 0;
    out[0] = 0;
    SendMessageTimeoutW(hwnd, WM_GETTEXT, (WPARAM)size, (LPARAM)out,
                        SMTO_ABORTIFHUNG, 3000, &result);
}

static int first;
static BOOL CALLBACK dump_child(HWND hwnd, LPARAM unused) {
    wchar_t name[128], value[32768];
    RECT rect;
    (void)unused;
    GetClassNameW(hwnd, name, 128);
    text(hwnd, value, 32768);
    GetWindowRect(hwnd, &rect);
    if (!first) putchar(',');
    first = 0;
    printf("{\"id\":%d,\"class\":", GetDlgCtrlID(hwnd)); json_string(name);
    fputs(",\"text\":", stdout); json_string(value);
    printf(",\"enabled\":%s,\"visible\":%s,\"rect\":[%ld,%ld,%ld,%ld]}",
        IsWindowEnabled(hwnd) ? "true" : "false", IsWindowVisible(hwnd) ? "true" : "false",
        rect.left, rect.top, rect.right, rect.bottom);
    return TRUE;
}

static HWND dialog_for(HWND main) {
    HWND dialog = NULL;
    while ((dialog = FindWindowExW(NULL, dialog, L"#32770", NULL))) {
        if (GetWindow(dialog, GW_OWNER) == main && IsWindowVisible(dialog)) return dialog;
    }
    return NULL;
}

int wmain(int argc, wchar_t **argv) {
    HWND main = FindWindowW(L"RustNFS.Native.Manager.0.1", NULL);
    if (!main) { fputs("{\"error\":\"RustNFS window missing\"}\n", stdout); return 2; }
    if (argc < 2) return 2;
    if (!wcscmp(argv[1], L"dump")) {
        wchar_t title[256]; RECT rect; DWORD pid = 0;
        text(main, title, 256); GetWindowRect(main, &rect); GetWindowThreadProcessId(main, &pid);
        fputs("{\"title\":", stdout); json_string(title);
        printf(",\"pid\":%lu,\"enabled\":%s,\"rect\":[%ld,%ld,%ld,%ld],\"controls\":[", pid,
            IsWindowEnabled(main) ? "true" : "false", rect.left, rect.top, rect.right, rect.bottom);
        first = 1; EnumChildWindows(main, dump_child, 0); puts("]}");
    } else if (!wcscmp(argv[1], L"set") && argc == 4) {
        HWND control = GetDlgItem(main, _wtoi(argv[2])); DWORD_PTR result = 0;
        if (!control || !IsWindowEnabled(control)) return 3;
        if (!SendMessageTimeoutW(control, WM_SETTEXT, 0, (LPARAM)argv[3], SMTO_ABORTIFHUNG, 3000, &result)) return 4;
        puts("{\"ok\":true}");
    } else if (!wcscmp(argv[1], L"click") && argc == 3) {
        HWND control = GetDlgItem(main, _wtoi(argv[2]));
        if (!control || !IsWindowEnabled(control)) return 3;
        if (!PostMessageW(control, BM_CLICK, 0, 0)) return 4;
        puts("{\"ok\":true}");
    } else if (!wcscmp(argv[1], L"dialog")) {
        HWND dialog = dialog_for(main);
        printf("{\"present\":%s,\"controls\":[", dialog ? "true" : "false");
        first = 1; if (dialog) EnumChildWindows(dialog, dump_child, 0); puts("]}");
    } else if (!wcscmp(argv[1], L"dismiss")) {
        HWND dialog = dialog_for(main);
        if (!dialog) return 3;
        PostMessageW(dialog, WM_COMMAND, argc > 2 ? _wtoi(argv[2]) : IDOK, 0); puts("{\"ok\":true}");
    } else if (!wcscmp(argv[1], L"clipboard")) {
        if (!OpenClipboard(main)) return 3;
        HANDLE data = GetClipboardData(CF_UNICODETEXT);
        wchar_t *value = data ? GlobalLock(data) : NULL;
        fputs("{\"text\":", stdout); json_string(value ? value : L""); puts("}");
        if (value) GlobalUnlock(data);
        CloseClipboard();
    } else if (!wcscmp(argv[1], L"close")) {
        PostMessageW(main, WM_CLOSE, 0, 0); puts("{\"ok\":true}");
    } else if (!wcscmp(argv[1], L"size") && argc == 4) {
        SetWindowPos(main, NULL, 0, 0, _wtoi(argv[2]), _wtoi(argv[3]), SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        puts("{\"ok\":true}");
    } else return 2;
    return 0;
}
