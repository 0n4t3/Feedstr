package top.dns7.feedstr;

import android.app.NativeActivity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.database.Cursor;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.PersistableBundle;
import android.provider.OpenableColumns;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.view.inputmethod.TextAttribute;
import android.window.OnBackInvokedDispatcher;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.HashMap;
import java.util.HashSet;

/**
 * Runs the Rust app (libfeedstr.so, from the feedstr-droid crate) and does the
 * few things it can't do itself, reporting back through the native methods.
 */
public class MainActivity extends NativeActivity {
    static {
        // So the native methods below are found in it.
        System.loadLibrary("feedstr");
    }

    /** Space taken by the system bars, cutouts and keyboard, in pixels. */
    private static native void nativeInsets(int left, int top, int right, int bottom);
    private static native void nativeBack();
    /** Typing on the keyboard (see TextInput): text, or one of the keys below with text null. */
    private static native void nativeTyped(int key, String text);
    private static final int TEXT = 0, BACKSPACE = 1, DELETE = 2, ENTER = 3;
    private static native void nativePaused();
    private static native void nativeResumed();
    /** data is null if the user cancelled. */
    private static native void nativeFileOpened(int request, byte[] data);
    /** place is null if the user cancelled. */
    private static native void nativeFileSaved(int request, String place);
    private static native void nativeFileFailed(int request, String message);
    /** A signer app's answer: the key, signed event or text; null if refused or cancelled. */
    private static native void nativeSignerAnswer(int request, String answer);

    private TextInput input;
    /** Files waiting for the user to pick where they go, by request. */
    private final HashMap<Integer, byte[]> toSave = new HashMap<>();
    /** Requests that went to a signer app. */
    private final HashSet<Integer> signerRequests = new HashSet<>();

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // Draw behind the system bars, and tell the app how much they cover.
        if (Build.VERSION.SDK_INT >= 28) {
            getWindow().getAttributes().layoutInDisplayCutoutMode =
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
        }
        View root = getWindow().getDecorView();
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
            root.setOnApplyWindowInsetsListener((view, insets) -> {
                Insets used = insets.getInsets(WindowInsets.Type.systemBars()
                        | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
                nativeInsets(used.left, used.top, used.right, used.bottom);
                return insets;
            });
        } else {
            root.setSystemUiVisibility(View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                    | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
            root.setOnApplyWindowInsetsListener((view, insets) -> {
                nativeInsets(insets.getSystemWindowInsetLeft(), insets.getSystemWindowInsetTop(),
                        insets.getSystemWindowInsetRight(), insets.getSystemWindowInsetBottom());
                return insets;
            });
        }
        // Take the keyboard's text: see TextInput.
        input = new TextInput(this);
        addContentView(input, new ViewGroup.LayoutParams(1, 1));
        input.requestFocus();
        // The app decides what back does (and leaves when there's nothing to go back from).
        if (Build.VERSION.SDK_INT >= 33) {
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                    OnBackInvokedDispatcher.PRIORITY_DEFAULT, MainActivity::nativeBack);
        }
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        nativeBack();
    }

    @Override
    protected void onPause() {
        super.onPause();
        nativePaused();
    }

    @Override
    protected void onResume() {
        super.onResume();
        nativeResumed();
    }

    // ---- Text from the keyboard ----

    /**
     * Stands in for NativeActivity's own view, which offers keyboards no way
     * to send text: they'd fall back to key presses, with no clipboard,
     * suggestions or dictation. What the keyboard does comes here instead and
     * goes to the app as typing, in order.
     */
    private static final class TextInput extends View {
        /** Whether a password (the Sync Key) is being typed. */
        boolean password;

        TextInput(Context context) {
            super(context);
            setFocusable(true);
            setFocusableInTouchMode(true);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        // The app shows and hides the keyboard for the whole window.
        @Override
        public boolean checkInputConnectionProxy(View view) {
            return view == getRootView();
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = InputType.TYPE_CLASS_TEXT | (password
                    ? InputType.TYPE_TEXT_VARIATION_PASSWORD
                    : InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
            info.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_FLAG_NO_EXTRACT_UI
                    | EditorInfo.IME_FLAG_NO_ENTER_ACTION
                    | (password ? EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING : 0);
            return new Typing(this);
        }
    }

    /**
     * Turns what the keyboard does into typing. The app doesn't tell the
     * keyboard what's in its text fields, so this keeps what was typed since
     * the keyboard (re)started, which is all the keyboard sees: enough to
     * compose words and replace them with suggestions.
     */
    private static final class Typing extends BaseInputConnection {
        private final View view;
        private final StringBuilder typed = new StringBuilder();
        /** Where the word being composed starts in typed, or -1. */
        private int composing = -1;

        Typing(View view) {
            super(view, false);
            this.view = view;
        }

        @Override
        public CharSequence getTextBeforeCursor(int n, int flags) {
            return typed.substring(Math.max(0, typed.length() - n));
        }

        @Override
        public CharSequence getTextAfterCursor(int n, int flags) {
            return "";
        }

        @Override
        public CharSequence getSelectedText(int flags) {
            return null;
        }

        @Override
        public boolean commitText(CharSequence text, int newCursorPosition) {
            replaceComposing(text.toString());
            composing = -1;
            moved();
            return true;
        }

        @Override
        public boolean setComposingText(CharSequence text, int newCursorPosition) {
            int start = composing >= 0 ? composing : typed.length();
            replaceComposing(text.toString());
            composing = start;
            moved();
            return true;
        }

        // How keyboards pick a suggestion, since Android 14.
        @Override
        public boolean replaceText(int start, int end, CharSequence text, int newCursorPosition,
                TextAttribute attribute) {
            if (!setComposingRegion(start, end)) {
                return false;
            }
            return commitText(text, newCursorPosition);
        }

        @Override
        public boolean setComposingRegion(int start, int end) {
            // Only a word just before the cursor, which is all that's known.
            if (end != typed.length() || start < 0 || start > end) {
                return false;
            }
            composing = start;
            moved();
            return true;
        }

        @Override
        public boolean finishComposingText() {
            composing = -1;
            moved();
            return true;
        }

        @Override
        public boolean deleteSurroundingText(int before, int after) {
            for (int i = 0; i < before; i++) {
                backspace();
            }
            for (int i = 0; i < after; i++) {
                nativeTyped(DELETE, null);
            }
            moved();
            return true;
        }

        @Override
        public boolean sendKeyEvent(KeyEvent event) {
            int key = event.getKeyCode();
            if (key != KeyEvent.KEYCODE_DEL && key != KeyEvent.KEYCODE_FORWARD_DEL
                    && key != KeyEvent.KEYCODE_ENTER) {
                return super.sendKeyEvent(event);
            }
            // In order with the text, rather than through the app's own key events.
            if (event.getAction() == KeyEvent.ACTION_DOWN) {
                composing = -1;
                if (key == KeyEvent.KEYCODE_DEL) {
                    backspace();
                } else {
                    nativeTyped(key == KeyEvent.KEYCODE_ENTER ? ENTER : DELETE, null);
                }
                moved();
            }
            return true;
        }

        @Override
        public boolean performEditorAction(int action) {
            nativeTyped(ENTER, null);
            return true;
        }

        @Override
        public boolean performContextMenuAction(int id) {
            if (id == android.R.id.paste || id == android.R.id.pasteAsPlainText) {
                String text = ((MainActivity) view.getContext()).getClipboardText();
                if (text != null) {
                    commitText(text, 1);
                }
                return true;
            }
            return super.performContextMenuAction(id);
        }

        private void replaceComposing(String text) {
            if (composing >= 0) {
                while (typed.length() > composing) {
                    backspace();
                }
            }
            if (text.equals("\n")) {
                nativeTyped(ENTER, null);
            } else if (!text.isEmpty()) {
                typed.append(text);
                nativeTyped(TEXT, text);
            }
        }

        /** Keyboards follow the cursor by being told where it is. */
        private void moved() {
            int end = typed.length();
            view.getContext().getSystemService(InputMethodManager.class)
                    .updateSelection(view, end, end, composing, composing >= 0 ? end : -1);
        }

        private void backspace() {
            if (typed.length() > 0) {
                typed.setLength(typed.length() - 1);
            }
            nativeTyped(BACKSPACE, null);
        }
    }

    /**
     * Called when the cursor moves to another text field or place, so the
     * keyboard starts afresh, and when a password starts or stops being typed.
     */
    public void restartInput(boolean password) {
        runOnUiThread(() -> {
            input.password = password;
            getSystemService(InputMethodManager.class).restartInput(input);
        });
    }

    // ---- The clipboard, which the app can't reach itself ----

    public String getClipboardText() {
        ClipboardManager clipboard = getSystemService(ClipboardManager.class);
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) {
            return null;
        }
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }

    public void setClipboardText(String text) {
        ClipData clip = ClipData.newPlainText("Feedstr", text);
        // Keep a copied Sync Key out of the clipboard preview.
        if (text.startsWith("nsec1") && Build.VERSION.SDK_INT >= 33) {
            PersistableBundle extras = new PersistableBundle();
            extras.putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true);
            clip.getDescription().setExtras(extras);
        }
        getSystemService(ClipboardManager.class).setPrimaryClip(clip);
    }

    // ---- Links, opened in other apps ----

    /** Opens a link (like nostr:naddr1…) in the app that handles it; false if none does. */
    public boolean openUri(String uri) {
        try {
            startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(uri)));
            return true;
        } catch (ActivityNotFoundException e) {
            return false;
        }
    }

    /** Offers a web link (an article's) to other apps through the system's share sheet. */
    public void shareLink(String title, String link) {
        runOnUiThread(() -> {
            Intent send = new Intent(Intent.ACTION_SEND)
                    .setType("text/plain")
                    .putExtra(Intent.EXTRA_SUBJECT, title)
                    .putExtra(Intent.EXTRA_TITLE, title)
                    .putExtra(Intent.EXTRA_TEXT, link);
            startActivity(Intent.createChooser(send, null));
        });
    }

    // ---- Files, asked for by the app ----

    public void openFile(int request) {
        runOnUiThread(() -> {
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                    .addCategory(Intent.CATEGORY_OPENABLE)
                    .setType("*/*");
            startActivityForResult(intent, request);
        });
    }

    public void saveFile(int request, String name, String mime, byte[] data) {
        runOnUiThread(() -> {
            toSave.put(request, data);
            Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT)
                    .addCategory(Intent.CATEGORY_OPENABLE)
                    .setType(mime)
                    .putExtra(Intent.EXTRA_TITLE, name);
            startActivityForResult(intent, request);
        });
    }

    // ---- Nostr signer apps (NIP-55), like Amber ----

    private static final String SIGNER_SCHEME = "nostrsigner:";

    /** Which signer app and account were chosen (excluded from backups like everything else). */
    private SharedPreferences signer() {
        return getSharedPreferences("signer", MODE_PRIVATE);
    }

    public boolean hasSignerApp() {
        Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(SIGNER_SCHEME));
        return !getPackageManager().queryIntentActivities(intent, 0).isEmpty();
    }

    /** The account's public key, if a signer app was chosen before. */
    public String signerPublicKey() {
        return signer().getString("pubkey", null);
    }

    public void forgetSigner() {
        signer().edit().clear().apply();
    }

    /** Asks for the user's key and, at the same time, for these permissions (NIP-55 JSON). */
    public void signerGetPublicKey(int request, String permissions) {
        runOnUiThread(() -> {
            Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(SIGNER_SCHEME))
                    .putExtra("type", "get_public_key")
                    .putExtra("permissions", permissions);
            signerRequests.add(request);
            startActivityForResult(intent, request);
        });
    }

    /**
     * Asks the signer app without showing it ("sign_event", "nip44_encrypt" or
     * "nip44_decrypt"). Returns "ok:" and the answer, "ask" if the app wants to
     * ask the user first (see signerAsk), or "rejected".
     */
    public String signerQuery(String type, String content, String pubkey) {
        SharedPreferences account = signer();
        String app = account.getString("package", null);
        if (app == null) {
            return "ask";
        }
        Uri uri = Uri.parse("content://" + app + "." + type.toUpperCase(java.util.Locale.ROOT));
        String[] question = {content, pubkey, account.getString("pubkey", "")};
        try (Cursor cursor = getContentResolver().query(uri, question, null, null, null)) {
            if (cursor == null) {
                return "ask";
            }
            if (cursor.getColumnIndex("rejected") >= 0) {
                return "rejected";
            }
            String answer = cursor.moveToFirst() ? answer(cursor, type) : null;
            return answer == null ? "ask" : "ok:" + answer;
        } catch (RuntimeException e) {
            return "ask";
        }
    }

    private static String answer(Cursor cursor, String type) {
        int event = cursor.getColumnIndex("event");
        int result = cursor.getColumnIndex("result");
        if (type.equals("sign_event") && event >= 0) {
            return cursor.getString(event);
        }
        return result >= 0 ? cursor.getString(result) : null;
    }

    /** Asks through the signer app's own screen; answers through nativeSignerAnswer. */
    public void signerAsk(int request, String type, String content, String pubkey) {
        runOnUiThread(() -> {
            SharedPreferences account = signer();
            Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(SIGNER_SCHEME + content))
                    .setPackage(account.getString("package", null))
                    .putExtra("type", type)
                    .putExtra("pubkey", pubkey)
                    .putExtra("current_user", account.getString("pubkey", ""));
            signerRequests.add(request);
            try {
                startActivityForResult(intent, request);
            } catch (ActivityNotFoundException e) {
                signerRequests.remove(request);
                nativeSignerAnswer(request, null);
            }
        });
    }

    private void signerAnswered(int request, int result, Intent intent) {
        String answer = null;
        if (result == RESULT_OK && intent != null) {
            String event = intent.getStringExtra("event");
            answer = event != null ? event : intent.getStringExtra("result");
            String app = intent.getStringExtra("package");
            if (app != null && answer != null) {
                // The answer to get_public_key: remember the app and account.
                signer().edit().putString("package", app).putString("pubkey", answer).apply();
            }
        }
        nativeSignerAnswer(request, answer);
    }

    @Override
    protected void onActivityResult(int request, int result, Intent intent) {
        super.onActivityResult(request, result, intent);
        if (signerRequests.remove(request)) {
            signerAnswered(request, result, intent);
            return;
        }
        Uri uri = result == RESULT_OK && intent != null ? intent.getData() : null;
        byte[] data = toSave.remove(request);
        // Reading and writing can be slow (cloud storage), so not on the UI thread.
        new Thread(() -> {
            try {
                if (data == null) {
                    nativeFileOpened(request, uri == null ? null : read(uri));
                } else if (uri == null) {
                    nativeFileSaved(request, null);
                } else {
                    write(uri, data);
                    nativeFileSaved(request, displayName(uri));
                }
            } catch (Exception e) {
                nativeFileFailed(request, String.valueOf(e.getMessage()));
            }
        }).start();
    }

    /** The biggest file imported, as MAX_IMPORT_BYTES in core. */
    private static final int MAX_IMPORT_BYTES = 100 << 20;

    private byte[] read(Uri uri) throws IOException {
        try (InputStream in = getContentResolver().openInputStream(uri)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buffer = new byte[64 * 1024];
            for (int n; (n = in.read(buffer)) > 0; ) {
                if (out.size() + n > MAX_IMPORT_BYTES) {
                    throw new IOException("That file is too big to import (over 100 MB)");
                }
                out.write(buffer, 0, n);
            }
            return out.toByteArray();
        }
    }

    private void write(Uri uri, byte[] data) throws IOException {
        try (OutputStream out = getContentResolver().openOutputStream(uri, "wt")) {
            out.write(data);
        }
    }

    private String displayName(Uri uri) {
        try (Cursor cursor = getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) {
                    return cursor.getString(column);
                }
            }
        }
        return uri.getLastPathSegment();
    }
}
