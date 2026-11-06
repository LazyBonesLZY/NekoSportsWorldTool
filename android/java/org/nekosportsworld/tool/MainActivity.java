package org.nekosportsworld.tool;

import android.app.AlertDialog;
import android.app.NativeActivity;
import android.os.Bundle;
import android.text.InputType;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;

/** Android UI thread owns the real text editor; Rust owns application state. */
public final class MainActivity extends NativeActivity {
    static { System.loadLibrary("nekosportsworldtool"); }
    private AlertDialog editorDialog;
    private boolean taskActive;
    private boolean resumed;

    private native void nativeSubmitEdit(long id, String value);
    private native void nativeSetInsets(int left, int top, int right, int bottom);

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        // NativeActivity's rendering surface must stay inside system bars.
        View content = findViewById(android.R.id.content);
        content.setOnApplyWindowInsetsListener((view, insets) -> {
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                android.graphics.Insets bars = insets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
                nativeSetInsets(bars.left, bars.top, bars.right, bars.bottom);
            } else {
                nativeSetInsets(insets.getSystemWindowInsetLeft(), insets.getSystemWindowInsetTop(),
                    insets.getSystemWindowInsetRight(), insets.getSystemWindowInsetBottom());
            }
            return insets;
        });
        content.requestApplyInsets();
        applySystemBarStyle();
    }

    @Override public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) applySystemBarStyle();
    }

    private void applySystemBarStyle() {
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            getWindow().getInsetsController().setSystemBarsAppearance(
                android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS |
                android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS,
                android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS |
                android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS);
        } else {
            getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);
        }
    }

    public void openEditor(long id, String initialValue, int kind) {
        runOnUiThread(() -> {
            if (isFinishing() || isDestroyed() || editorDialog != null) return;
            EditText input = new EditText(this);
            input.setId(android.R.id.edit);
            int inputType;
            switch (kind) {
                case 1: inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD; break;
                case 2: inputType = InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_SIGNED; break;
                case 3: inputType = InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_SIGNED | InputType.TYPE_NUMBER_FLAG_DECIMAL; break;
                default: inputType = InputType.TYPE_CLASS_TEXT;
            }
            input.setInputType(inputType);
            input.setSingleLine(true);
            input.setImeOptions(EditorInfo.IME_ACTION_DONE | EditorInfo.IME_FLAG_NO_EXTRACT_UI);
            input.setText(initialValue);
            input.selectAll();
            int margin = Math.round(20 * getResources().getDisplayMetrics().density);
            FrameLayout container = new FrameLayout(this);
            container.setPadding(margin, 0, margin, 0);
            container.addView(input, new FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.WRAP_CONTENT));
            AlertDialog dialog = new AlertDialog.Builder(this)
                .setTitle(kind == 1 ? "输入密码" : "编辑内容")
                .setView(container)
                .setPositiveButton("确定", (ignored, which) -> nativeSubmitEdit(id, input.getText().toString()))
                .setNegativeButton("取消", null)
                .create();
            editorDialog = dialog;
            dialog.setOnDismissListener(ignored -> {
                InputMethodManager ime = (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                ime.hideSoftInputFromWindow(input.getWindowToken(), 0);
                editorDialog = null;
            });
            input.setOnEditorActionListener((view, action, event) -> {
                if (action != EditorInfo.IME_ACTION_DONE) return false;
                nativeSubmitEdit(id, input.getText().toString());
                dialog.dismiss();
                return true;
            });
            dialog.setOnShowListener(ignored -> {
                input.requestFocus();
                dialog.getWindow().setSoftInputMode(
                    WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_VISIBLE |
                    WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
                input.post(() -> ((InputMethodManager) getSystemService(INPUT_METHOD_SERVICE))
                    .showSoftInput(input, InputMethodManager.SHOW_IMPLICIT));
            });
            dialog.show();
        });
    }

    public void setTaskActive(boolean active) {
        runOnUiThread(() -> {
            taskActive = active;
            updateScreenPolicy();
        });
    }

    public void copyText(String text) {
        runOnUiThread(() -> {
            android.content.ClipboardManager clipboard =
                (android.content.ClipboardManager) getSystemService(CLIPBOARD_SERVICE);
            clipboard.setPrimaryClip(android.content.ClipData.newPlainText("NekoSportsWorldTool", text));
        });
    }

    /** 自更新：把私有目录里已下载的 update.apk 交给系统安装器。 */
    public void installApk(String path) {
        runOnUiThread(() -> {
            try {
                if (path == null || !new java.io.File(path).isFile()) {
                    android.widget.Toast.makeText(this,
                        "安装包不存在，请重新检查更新",
                        android.widget.Toast.LENGTH_LONG).show();
                    return;
                }
                android.content.Intent intent = new android.content.Intent(android.content.Intent.ACTION_VIEW);
                intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
                intent.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION);
                intent.setDataAndType(ApkProvider.uriForUpdate(), "application/vnd.android.package-archive");
                startActivity(intent);
            } catch (Exception error) {
                android.widget.Toast.makeText(this,
                    "无法启动安装器，请到系统设置允许本应用安装未知应用后重试",
                    android.widget.Toast.LENGTH_LONG).show();
            }
        });
    }

    /** 当前 APK 的 versionName（与 Release tag 对齐，供更新检查比较）。 */
    public String appVersionName() {
        try {
            return getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
        } catch (Exception error) {
            return "";
        }
    }

    private void updateScreenPolicy() {
        if (taskActive && resumed) getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        else getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
    }

    @Override protected void onResume() {
        super.onResume();
        resumed = true;
        updateScreenPolicy();
    }

    @Override protected void onPause() {
        resumed = false;
        updateScreenPolicy();
        super.onPause();
    }

    @Override protected void onDestroy() {
        if (editorDialog != null) editorDialog.dismiss();
        taskActive = false;
        updateScreenPolicy();
        super.onDestroy();
    }
}
