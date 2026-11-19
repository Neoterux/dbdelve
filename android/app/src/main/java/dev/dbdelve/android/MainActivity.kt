package dev.dbdelve.android

import android.os.Bundle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_WEAK
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import androidx.biometric.BiometricPrompt
import androidx.compose.material3.Surface
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import dev.dbdelve.android.data.ProfileStore
import dev.dbdelve.android.data.Secrets
import dev.dbdelve.android.ui.ConnectionForm
import dev.dbdelve.android.ui.ConnectionsScreen
import dev.dbdelve.android.ui.DBDelveTheme
import dev.dbdelve.android.ui.WorkspaceScreen

// A FragmentActivity because BiometricPrompt needs one.
class MainActivity : FragmentActivity() {
    private val model: AppViewModel by viewModels {
        viewModelFactory {
            initializer {
                AppViewModel(ProfileStore(filesDir), Secrets(applicationContext)) { connection ->
                    QueryService.keepAlive(applicationContext, connection)
                }
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            DBDelveTheme {
                Surface {
                    when (val screen = model.screen) {
                        Screen.Connections -> ConnectionsScreen(model)
                        is Screen.Edit -> {
                            BackHandler { model.screen = Screen.Connections }
                            ConnectionForm(model, screen.profile, screen.duplicate)
                        }
                        Screen.Workspace -> WorkspaceScreen(model, ::authenticate)
                    }
                }
            }
        }
    }

    /**
     * The device's own lock before a connection goes above its mode. Weak
     * biometrics or the PIN are enough: this guards against a stray tap, not
     * a stolen phone, which the lock screen already does.
     */
    private fun authenticate(title: String, onSuccess: () -> Unit) {
        val allowed = BIOMETRIC_WEAK or DEVICE_CREDENTIAL
        if (BiometricManager.from(this).canAuthenticate(allowed) != BiometricManager.BIOMETRIC_SUCCESS) {
            // No screen lock set: the dialog's own tap is all there is to ask.
            onSuccess()
            return
        }
        val prompt = BiometricPrompt(
            this,
            ContextCompat.getMainExecutor(this),
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) = onSuccess()
            },
        )
        prompt.authenticate(
            BiometricPrompt.PromptInfo.Builder()
                .setTitle(title)
                .setAllowedAuthenticators(allowed)
                .build(),
        )
    }
}
