#include <Security/Security.h>
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>
#include <string.h>
/* Only synthetic public data. The supplied Keychain is new and temporary. */
static SecKeychainRef keychain;
static CFMutableDictionaryRef query(CFStringRef account, int add) {
    CFMutableDictionaryRef d = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(d, kSecClass, kSecClassGenericPassword);
    CFDictionarySetValue(d, kSecAttrService, CFSTR("cadence.synthetic.pair"));
    CFDictionarySetValue(d, kSecAttrAccount, account);
    CFDictionarySetValue(d, kSecAttrSynchronizable, kCFBooleanFalse);
    if (add) CFDictionarySetValue(d, kSecUseKeychain, keychain);
    else { const void *one[] = {keychain}; CFArrayRef list = CFArrayCreate(NULL, one, 1, &kCFTypeArrayCallBacks); CFDictionarySetValue(d, kSecMatchSearchList, list); CFRelease(list); }
    return d;
}
static OSStatus create(CFStringRef account, const char *text) {
    CFMutableDictionaryRef d = query(account, 1); CFDataRef data = CFDataCreate(NULL, (const UInt8 *)text, strlen(text));
    CFDictionarySetValue(d, kSecValueData, data); OSStatus status = SecItemAdd(d, NULL); CFRelease(data); CFRelease(d); return status;
}
static OSStatus update(CFStringRef account, const char *text) {
    CFMutableDictionaryRef q = query(account, 0); CFMutableDictionaryRef d = CFDictionaryCreateMutable(NULL,0,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    CFDataRef value = CFDataCreate(NULL,(const UInt8 *)text,strlen(text)); CFDictionarySetValue(d,kSecValueData,value);
    OSStatus status = SecItemUpdate(q,d); CFRelease(value); CFRelease(d); CFRelease(q); return status;
}
static OSStatus remove_item(CFStringRef account) { CFMutableDictionaryRef q=query(account,0); OSStatus status=SecItemDelete(q); CFRelease(q); return status; }
static int pointer_is(const char *expected) { CFMutableDictionaryRef q=query(CFSTR("pointer"),0); CFDictionarySetValue(q,kSecReturnData,kCFBooleanTrue); CFTypeRef out=NULL; OSStatus status=SecItemCopyMatching(q,&out); int ok=status==0 && out && CFGetTypeID(out)==CFDataGetTypeID() && CFDataGetLength(out)==1 && *CFDataGetBytePtr(out)==*expected; if(out)CFRelease(out); CFRelease(q); return ok; }
int main(int argc,char **argv) {
    if(argc!=2)return 2; const char *password="synthetic-only"; OSStatus status=SecKeychainCreate(argv[1],14,password,0,NULL,&keychain); if(status){printf("create_keychain=%d\n",(int)status);return 1;}
    int ok = create(CFSTR("A:certificate"),"public-A")==0 && create(CFSTR("A:password"),"public-pass-A")==0 && create(CFSTR("pointer"),"A")==0 && pointer_is("A");
    /* Simulate a failed second staged write: remove B and retain A. */
    ok = ok && create(CFSTR("B:certificate"),"public-B")==0 && remove_item(CFSTR("B:certificate"))==0 && pointer_is("A");
    ok = ok && create(CFSTR("B:certificate"),"public-B")==0 && create(CFSTR("B:password"),"public-pass-B")==0 && update(CFSTR("pointer"),"B")==0 && pointer_is("B");
    OSStatus a=remove_item(CFSTR("A:certificate")), b=remove_item(CFSTR("A:password")), c=remove_item(CFSTR("B:certificate")), d=remove_item(CFSTR("B:password")), e=remove_item(CFSTR("pointer"));
    status=SecKeychainDelete(keychain); CFRelease(keychain); printf("pair_staging_and_replacement=%s cleanup_osstatus=%d,%d,%d,%d,%d keychain_delete_osstatus=%d\n",ok?"pass":"failed",(int)a,(int)b,(int)c,(int)d,(int)e,(int)status);
    return ok && a==0 && b==0 && c==0 && d==0 && e==0 && status==0 ? 0 : 1;
}
