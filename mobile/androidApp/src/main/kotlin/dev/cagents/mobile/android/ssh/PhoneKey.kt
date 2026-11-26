package dev.cagents.mobile.android.ssh

import android.content.Context
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.Base64
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.io.File
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.SecureRandom
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The phone's own Ed25519 SSH key, made on first use and never exported.
 *
 * Where the Android Keystore can hold an Ed25519 key (Android 13 and later
 * with KeyMint 2), the private key lives there and signing happens in the
 * secure hardware. Otherwise the key is generated in software and its seed is
 * stored encrypted with an AES key that the Keystore holds.
 *
 * A key that exists is never replaced without the person asking, since
 * computers trust it: if it is damaged or gone, loading it fails with
 * [PhoneKeyUnusable], and if the Keystore fails otherwise, with that
 * failure, until the person makes a new one with [replace].
 */
class PhoneKey private constructor(
    override val publicKey: ByteArray,
    private val signer: (ByteArray) -> ByteArray,
    /** True when the private key stays inside the Android Keystore. */
    val hardwareBacked: Boolean,
) : SigningKey {
    override fun sign(data: ByteArray): ByteArray = signer(data)

    /** The key as one `authorized_keys` entry, without options. */
    val openSsh: String
        get() = "$openSshKey $COMMENT"

    companion object {
        const val COMMENT = "cagents-phone"
        private const val KEYSTORE = "AndroidKeyStore"
        private const val SIGNING_ALIAS = "cagents-ssh-ed25519"
        private const val WRAPPING_ALIAS = "cagents-ssh-seed-wrap"
        private const val SEED_FILE = "ssh-ed25519.seed"

        @Volatile
        private var loaded: PhoneKey? = null

        /**
         * Loads the phone's key, making it on first use. Blocking: call off
         * the main thread. Throws [PhoneKeyUnusable] if the key it has can't
         * be used.
         */
        fun get(context: Context): PhoneKey = loaded ?: synchronized(this) {
            loaded ?: load(context.applicationContext).also { loaded = it }
        }

        /**
         * Makes a new key in place of one that fails to load, for whatever
         * reason. A key that loads now is kept instead, since computers may
         * trust it. Computers that trusted a replaced key won't accept the
         * new one until it is added there. Blocking: call off the main thread.
         */
        fun replace(context: Context): Replacement = synchronized(this) {
            val app = context.applicationContext
            loaded?.let { return Replacement(it, made = false) }
            runCatching { load(app) }.onSuccess {
                loaded = it
                return Replacement(it, made = false)
            }
            // Deleting an entry that isn't there succeeds; any other failure leaves the old key for another try.
            KeyStore.getInstance(KEYSTORE).apply { load(null) }.run {
                deleteEntry(SIGNING_ALIAS)
                deleteEntry(WRAPPING_ALIAS)
            }
            val seedFile = File(app.noBackupFilesDir, SEED_FILE)
            File(seedFile.parentFile, "${seedFile.name}.tmp").delete()
            check(!seedFile.exists() || seedFile.delete()) { "Couldn't remove the old key" }
            Replacement(load(app).also { loaded = it }, made = true)
        }

        private fun load(context: Context): PhoneKey {
            val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
            val seedFile = File(context.noBackupFilesDir, SEED_FILE)
            if (!seedFile.exists()) {
                keystoreKey(keyStore)?.let { return it }
            }
            return softwareKey(keyStore, seedFile)
        }

        private fun keystoreKey(keyStore: KeyStore): PhoneKey? {
            if (android.os.Build.VERSION.SDK_INT < 33) return null
            val existed = keyStore.containsAlias(SIGNING_ALIAS)
            return runCatching {
                if (!existed) {
                    KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, KEYSTORE).apply {
                        initialize(
                            KeyGenParameterSpec.Builder(SIGNING_ALIAS, KeyProperties.PURPOSE_SIGN)
                                .setAlgorithmParameterSpec(ECGenParameterSpec("ed25519"))
                                .setDigests(KeyProperties.DIGEST_NONE)
                                .build(),
                        )
                    }.generateKeyPair()
                }
                val private = keyStore.getKey(SIGNING_ALIAS, null) as PrivateKey?
                    ?: throw PhoneKeyUnusable(IllegalStateException("The Keystore no longer holds the key."))
                val encoded = keyStore.getCertificate(SIGNING_ALIAS)?.publicKey?.encoded
                    ?: throw PhoneKeyUnusable(IllegalStateException("The Keystore no longer holds the key's certificate."))
                val raw = encoded.copyOfRange(encoded.size - 32, encoded.size)
                val key = PhoneKey(raw, { data -> keystoreSign(private, data) }, hardwareBacked = true)
                // Some devices create the key but can't sign with it; find out now.
                key.sign(byteArrayOf(0))
                key
            }.onFailure {
                // Only a key made just now is discarded: an older one is already authorized somewhere.
                if (!existed) runCatching { keyStore.deleteEntry(SIGNING_ALIAS) }
            }.getOrElse { if (existed) throw classified(it) else null }
        }

        private fun keystoreSign(key: PrivateKey, data: ByteArray): ByteArray =
            Signature.getInstance("Ed25519").run {
                initSign(key)
                update(data)
                sign()
            }

        private fun softwareKey(keyStore: KeyStore, seedFile: File): PhoneKey {
            val seed = if (seedFile.exists()) {
                // A missing wrapping key isn't made afresh here: it couldn't open this seed.
                val wrapping = try {
                    keyStore.getKey(WRAPPING_ALIAS, null) as SecretKey?
                } catch (e: Exception) {
                    throw classified(e)
                }
                unsealSeed(seedFile.readBytes(), wrapping)
            } else {
                val wrapping = wrappingKey(keyStore)
                val fresh = ByteArray(32).also { SecureRandom().nextBytes(it) }
                val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, wrapping) }
                val sealed = cipher.iv + cipher.doFinal(fresh)
                val temporary = File(seedFile.parentFile, "${seedFile.name}.tmp")
                temporary.writeBytes(sealed)
                check(temporary.renameTo(seedFile)) { "Couldn't store the phone's key" }
                fresh
            }
            val private = Ed25519PrivateKeyParameters(seed, 0)
            val public = private.generatePublicKey().encoded
            return PhoneKey(public, { data ->
                Ed25519Signer().run {
                    init(true, private)
                    update(data, 0, data.size)
                    generateSignature()
                }
            }, hardwareBacked = false)
        }

        /**
         * The seed in [stored], as [softwareKey] seals it: the GCM IV, then the
         * encrypted seed and its tag. Throws [PhoneKeyUnusable] when [stored]
         * is damaged or [wrapping] is gone or can't open it; other failures,
         * which may pass, are thrown as they are.
         */
        internal fun unsealSeed(
            stored: ByteArray,
            wrapping: SecretKey?,
            keystoreError: (Throwable) -> Int? = ::keystoreErrorCode,
        ): ByteArray {
            if (wrapping == null) throw PhoneKeyUnusable(IllegalStateException("The key that sealed it is gone."))
            if (stored.size != SEALED_SIZE) {
                throw PhoneKeyUnusable(IllegalStateException("The stored seed is ${stored.size} bytes, not $SEALED_SIZE."))
            }
            return try {
                Cipher.getInstance("AES/GCM/NoPadding").run {
                    init(Cipher.DECRYPT_MODE, wrapping, GCMParameterSpec(TAG_BITS, stored, 0, IV_SIZE))
                    doFinal(stored, IV_SIZE, stored.size - IV_SIZE)
                }
            } catch (e: Exception) {
                throw classified(e, keystoreError)
            }
        }

        private const val IV_SIZE = 12
        private const val TAG_BITS = 128
        private const val SEALED_SIZE = IV_SIZE + 32 + TAG_BITS / 8

        /** [failure] as [PhoneKeyUnusable] if [keyIsBroken] says so, else as it is. */
        private fun classified(failure: Throwable, keystoreError: (Throwable) -> Int? = ::keystoreErrorCode): Throwable =
            when {
                failure is PhoneKeyUnusable -> failure
                keyIsBroken(failure, keystoreError) -> PhoneKeyUnusable(failure)
                else -> failure
            }

        private fun wrappingKey(keyStore: KeyStore): SecretKey {
            (keyStore.getKey(WRAPPING_ALIAS, null) as? SecretKey)?.let { return it }
            return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE).apply {
                init(
                    KeyGenParameterSpec.Builder(
                        WRAPPING_ALIAS,
                        KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                    )
                        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                        .setKeySize(256)
                        .build(),
                )
            }.generateKey()
        }

        /** The SSH wire encoding of an Ed25519 public key: its type and its 32 bytes. */
        fun publicBlob(raw: ByteArray): ByteArray = ByteArrayOutputStream().also { bytes ->
            DataOutputStream(bytes).run {
                val type = "ssh-ed25519".toByteArray()
                writeInt(type.size)
                write(type)
                writeInt(raw.size)
                write(raw)
            }
        }.toByteArray()
    }
}

/** The key [replace][PhoneKey.replace] returned, and whether it is new rather than the old one, working again. */
class Replacement(val key: PhoneKey, val made: Boolean)

/**
 * Whether [failure], from loading the phone's key, shows that the key itself
 * is damaged or gone, so that only a new key helps, rather than that the
 * Keystore failed for now: busy, not yet unlocked, or its service restarting.
 * [keystoreError] reads the Keystore's error code from one exception in the
 * chain, if it is a Keystore error.
 */
internal fun keyIsBroken(failure: Throwable, keystoreError: (Throwable) -> Int? = ::keystoreErrorCode): Boolean {
    val chain = generateSequence(failure) { it.cause?.takeIf { cause -> cause !== it } }.take(MAX_CAUSES).toList()
    // Data sealed with another key or changed since, or a key the system invalidated for good.
    if (chain.any { it is PhoneKeyUnusable || it is AEADBadTagException || it is KeyPermanentlyInvalidatedException }) {
        return true
    }
    val code = chain.firstNotNullOfOrNull(keystoreError) ?: return false
    return code == KEYSTORE_KEY_DOES_NOT_EXIST || code == KEYSTORE_KEY_CORRUPTED
}

private const val MAX_CAUSES = 16

// android.security.KeyStoreException.ERROR_KEY_DOES_NOT_EXIST and ERROR_KEY_CORRUPTED, of API 33.
private const val KEYSTORE_KEY_DOES_NOT_EXIST = 6
private const val KEYSTORE_KEY_CORRUPTED = 7

/** The Keystore's error code in [failure], where Android exposes it (API 33 and later). */
private fun keystoreErrorCode(failure: Throwable): Int? =
    if (Build.VERSION.SDK_INT >= 33 && failure is android.security.KeyStoreException) failure.numericErrorCode else null

/** The phone's key exists but is damaged or gone, such as after the Keystore lost what it held. */
class PhoneKeyUnusable(cause: Throwable) :
    Exception("This phone's key can't be used anymore. Make a new one in Settings.", cause)
