import { dlopen, FFIType, ptr, toArrayBuffer } from "bun:ffi"

const errSecSuccess = 0
const errSecItemNotFound = -25300
const errSecDuplicateItem = -25299

function readPtr(buf: Uint8Array): number {
  return Number(new DataView(buf.buffer, buf.byteOffset).getBigUint64(0, true))
}

let _sym: ReturnType<typeof loadLib>["symbols"] | undefined

function sym() {
  if (!_sym) _sym = loadLib().symbols
  return _sym
}

function loadLib() {
  return dlopen("/System/Library/Frameworks/Security.framework/Security", {
    SecKeychainAddGenericPassword: {
      args: [FFIType.ptr, FFIType.u32, FFIType.ptr, FFIType.u32, FFIType.ptr, FFIType.u32, FFIType.ptr, FFIType.ptr],
      returns: FFIType.i32,
    },
    SecKeychainFindGenericPassword: {
      args: [FFIType.ptr, FFIType.u32, FFIType.ptr, FFIType.u32, FFIType.ptr, FFIType.ptr, FFIType.ptr, FFIType.ptr],
      returns: FFIType.i32,
    },
    SecKeychainItemModifyAttributesAndData: {
      args: [FFIType.ptr, FFIType.ptr, FFIType.u32, FFIType.ptr],
      returns: FFIType.i32,
    },
    SecKeychainItemFreeContent: {
      args: [FFIType.ptr, FFIType.ptr],
      returns: FFIType.i32,
    },
    SecKeychainItemDelete: {
      args: [FFIType.ptr],
      returns: FFIType.i32,
    },
    CFRelease: {
      args: [FFIType.ptr],
      returns: FFIType.void,
    },
  } as const)
}

export function keychainGet(service: string, account: string): string | undefined {
  const svc = Buffer.from(service)
  const acc = Buffer.from(account)
  const lenBuf = new Uint8Array(4)
  const dataBuf = new Uint8Array(8)

  const s = sym().SecKeychainFindGenericPassword(
    null,
    svc.byteLength, ptr(svc),
    acc.byteLength, ptr(acc),
    ptr(lenBuf), ptr(dataBuf),
    null,
  ) as number

  if (s === errSecItemNotFound) return undefined
  if (s !== errSecSuccess) throw keychainError("read", s)

  const dataPtr = readPtr(dataBuf)
  const dataLen = new DataView(lenBuf.buffer).getUint32(0, true)
  const result = Buffer.from(toArrayBuffer(dataPtr, 0, dataLen)).toString("utf8")
  sym().SecKeychainItemFreeContent(null, dataPtr)
  return result
}

export function keychainSet(service: string, account: string, password: string): void {
  const svc = Buffer.from(service)
  const acc = Buffer.from(account)
  const pwd = Buffer.from(password)
  const itemBuf = new Uint8Array(8)

  let s = sym().SecKeychainAddGenericPassword(
    null,
    svc.byteLength, ptr(svc),
    acc.byteLength, ptr(acc),
    pwd.byteLength, ptr(pwd),
    ptr(itemBuf),
  ) as number

  if (s === errSecSuccess) {
    const ref = readPtr(itemBuf)
    if (ref) sym().CFRelease(ref)
    return
  }

  if (s !== errSecDuplicateItem) throw keychainError("add", s)

  // Item exists — find it and update in place
  const itemBuf2 = new Uint8Array(8)
  s = sym().SecKeychainFindGenericPassword(
    null,
    svc.byteLength, ptr(svc),
    acc.byteLength, ptr(acc),
    null, null,
    ptr(itemBuf2),
  ) as number
  if (s !== errSecSuccess) throw keychainError("find for update", s)

  const itemRef = readPtr(itemBuf2)
  s = sym().SecKeychainItemModifyAttributesAndData(itemRef, null, pwd.byteLength, ptr(pwd)) as number
  sym().CFRelease(itemRef)
  if (s !== errSecSuccess) throw keychainError("update", s)
}

export function keychainDelete(service: string, account: string): void {
  const svc = Buffer.from(service)
  const acc = Buffer.from(account)
  const itemBuf = new Uint8Array(8)

  const s = sym().SecKeychainFindGenericPassword(
    null,
    svc.byteLength, ptr(svc),
    acc.byteLength, ptr(acc),
    null, null,
    ptr(itemBuf),
  ) as number

  if (s === errSecItemNotFound) return
  if (s !== errSecSuccess) throw keychainError("find for delete", s)

  const itemRef = readPtr(itemBuf)
  sym().SecKeychainItemDelete(itemRef)
  sym().CFRelease(itemRef)
}

function keychainError(op: string, code: number): Error {
  const err = new Error(`Keychain ${op} failed: ${code}`) as Error & { code: number }
  err.code = code
  return err
}
