# PhotoSync — Đặc tả sản phẩm & kỹ thuật (bản tiếng Việt)

> Bản dịch để đọc hiểu. Khi triển khai, `app_info.md` (tiếng Anh) là bản chuẩn — nếu hai file lệch nhau thì tin bản tiếng Anh.
> Mục §24 là những điểm **chưa quyết**; mọi mục còn lại đã chốt và có thể làm theo đúng như viết.

---

## 1. Sản phẩm trong một câu

PhotoSync copy toàn bộ thư viện ảnh/video của một điện thoại sang điện thoại khác qua mạng nội bộ — không cloud, không server, không tài khoản — và lưu media nhận được vào đúng ứng dụng Photos/Gallery của máy đích.

Ghép máy dễ như LocalSend (một mã số). Truyền dữ liệu đầy đủ như một lần backup Immich (cả thư viện, chỉ gửi phần thiếu, tiếp tục được sau khi mất kết nối, có kiểm tra toàn vẹn).

---

## 2. Lấy gì, từ đâu

| Từ LocalSend | Từ Immich |
|---|---|
| Truyền trực tiếp máy sang máy qua LAN | Sync cả thư viện thay vì tự chọn từng file |
| Ghép máy bằng mã số đơn giản | Sync tăng dần (chỉ gửi cái còn thiếu) |
| Vai trò Gửi / Nhận rõ ràng | Định danh ảnh theo nội dung, không theo tên file |
| UI cực kỳ đơn giản | Truyền video lớn theo chunk, tiếp tục được |
| Không tài khoản, không server | Kiểm tra toàn vẹn trước khi lưu |
| Chạy được khi một máy phát Wi-Fi | Trạng thái sync bền, sống qua lần restart app |

PhotoSync **không** phải app chia sẻ file nói chung, và **không** phải nền tảng quản lý ảnh. Nó là công cụ copy thư viện một chiều với trải nghiệm ngang LocalSend.

---

## 3. Những thứ dứt khoát không làm

Không có trong sản phẩm này, kể cả về sau:

- Cloud, tài khoản, đăng nhập, server trung tâm
- PostgreSQL, Redis, bất kỳ backend nào
- UI gallery / timeline / quản lý album
- AI, nhận diện mặt, tìm kiếm ngữ nghĩa
- Transcode video
- Tính năng xã hội, link chia sẻ, bình luận

Không có trong MVP (xem §21):

- Sync hai chiều
- Đồng bộ việc xoá ảnh (tombstone)
- Giữ album và mục yêu thích
- Sync nền / tự động
- Bản desktop (Windows / Linux)

---

## 4. Nền tảng

MVP: **iOS ↔ Android**, đủ bốn tổ hợp hướng (iOS→Android, Android→iOS, iOS→iOS, Android→Android).

Desktop ngoài phạm vi, nhưng sync engine và giao thức phải độc lập nền tảng để sau này còn làm được. Không dòng code nào trong engine được gọi trực tiếp PhotoKit hay MediaStore (xem §16).

---

## 5. Vai trò: Gửi và Nhận

Không có thương lượng vai trò. Người dùng tự chọn.

```
Máy A                            Máy B
tab [ Gửi ]                      tab [ Nhận ]
   │                                │
   │  nhập mã mà B đang hiện        │  hiện mã 6 số, đứng chờ
   └────────────► kết nối ──────────┘
   bên gửi                          bên nhận
```

Quy tắc:

- **Bên nhận** là bên thụ động. Nó hiện mã và lắng nghe.
- **Bên gửi** là bên chủ động. Nó nhập mã, rồi chọn gửi gì (mặc định: tất cả).
- Một máy không bao giờ đóng cả hai vai cùng lúc. Đổi tab là huỷ phiên đang chạy ở tab kia.
- Chỉ đọc thư viện của bên gửi. Chỉ ghi vào thư viện của bên nhận.

Muốn copy cả hai chiều thì hai người đổi vai và chạy lại lần nữa. Đây là giới hạn có chủ ý của MVP, không phải lỗi.

---

## 6. Ghép máy bằng mã số

Bên nhận sinh một **mã 6 số**, hiện to trên màn hình.

```
┌─────────────────────────────┐
│                             │
│        Sẵn sàng nhận        │
│                             │
│          4 8 2  9 1 3       │
│                             │
│   Nhập mã này trên máy gửi  │
│                             │
│   Lưu vào: Photos           │
│                             │
│         [ Huỷ ]              │
│                             │
└─────────────────────────────┘
```

Tính chất của mã:

- 6 số, sinh ngẫu nhiên mỗi phiên
- Dùng một lần, hết hiệu lực sau 5 phút không có kết nối
- Tối đa 3 lần nhập sai, sau đó bên nhận sinh mã mới
- Đọc được thành tiếng, nên hai người ở cách nhau vài mét vẫn dùng được

Mã làm hai việc cùng lúc: xác định **máy nào** cần kết nối tới (§7) và chứng minh kết nối đúng máy chứ không phải máy lạ (§9).

Tuỳ chọn (không chặn MVP): hiện thêm QR chứa cùng mã đó kèm địa chỉ, để bên gửi quét thay vì gõ.

---

## 7. Cấu hình mạng và cách tìm thấy máy kia

Sau khi bên nhận bắt đầu lắng nghe, có ba cách tìm ra nó. Bên gửi thử lần lượt.

**7.1 mDNS / Bonjour (cách chính)**

Bên nhận quảng bá service `_photosync._tcp`, trong TXT record có mã phiên và phiên bản giao thức. Bên gửi liệt kê service và khớp với mã người dùng vừa gõ.

- iOS: `NWListener` / `NWBrowser`. Bắt buộc khai `NSBonjourServices` trong `Info.plist` (từ iOS 14), nếu không việc dò tìm sẽ trả về rỗng mà không báo lỗi gì.
- Android: `NsdManager`.

**7.2 Quét subnet (dự phòng)**

Nếu mDNS không trả về gì trong khoảng 3 giây, bên gửi quét dải /24 của chính nó trên port cố định của PhotoSync, mở nhiều kết nối song song (~250 socket, timeout ngắn). Máy nào trả lời thì bắt tay, rồi mã 6 số quyết định đó có phải máy đúng không.

Bước dự phòng này tồn tại chính là để phục vụ trường hợp hotspot, nơi multicast không đáng tin.

**7.3 Nhập địa chỉ tay (phương án cuối, ẩn sau nút "Không tìm thấy?")**

Người dùng gõ IP đang hiện trên máy nhận.

**7.4 Chế độ hotspot**

Tình huống bắt buộc phải chạy được: "một máy phát Wi-Fi, máy kia vào", hoàn toàn không có internet.

```
Máy A (phát hotspot, 192.168.43.1)
        ▲
        │  Wi-Fi, không internet
        ▼
Máy B (máy con, 192.168.43.x)
```

Máy nào phát hotspot cũng được, máy nào gửi cũng được. App tuyệt đối không được coi "không có đường ra internet" là "không có mạng" — đây là lỗi rất hay gặp: hệ điều hành đánh dấu interface là không dùng được và app bỏ cuộc. Phải bind thẳng vào interface nội bộ.

> Lưu ý: iPhone không thể tự vào Personal Hotspot của chính nó, và iOS giới hạn khá nhiều thứ với máy con của hotspot. Cấu hình chắc chắn chạy là **Android phát hotspot, iPhone làm máy con**, hoặc cả hai vào cùng một mạng Wi-Fi thường. Điểm này phải kiểm chứng sớm trên máy thật (§20).

---

## 8. Tầng truyền

- TCP, một stream điều khiển và N stream dữ liệu (bắt đầu với N=1, để cấu hình được)
- Frame nhị phân có tiền tố độ dài, kèm header JSON/CBOR nhỏ cho mỗi message
- Tầng truyền nằm sau interface `TransportProvider`. Sau này thêm Wi-Fi Direct hay USB không phải sửa sync engine.

---

## 9. Bảo mật

LAN là môi trường không tin được. Một hotspot điện thoại ở quán cà phê là mạng thù địch.

- **TLS 1.3** cho kết nối dữ liệu. Bên nhận giữ một cặp khoá tự ký, sinh ở lần chạy đầu, lưu trong Keychain / keystore của hệ điều hành.
- **Xác thực ràng buộc theo mã.** Sau khi bắt tay TLS, hai bên trao đổi `HMAC-SHA256(KDF(mã), transcript)`, trong đó transcript bao gồm fingerprint của certificate TLS. Kẻ đứng giữa dùng certificate của chính nó sẽ tạo ra HMAC không khớp và bị từ chối.
- Mã không bao giờ được gửi qua đường truyền, dưới bất kỳ dạng nào.
- 6 số là không gian nhỏ, nên được bảo vệ bằng: dùng một lần, hết hạn sau 5 phút, giới hạn 3 lần thử, và sinh mã mới sau mỗi lần thất bại.

Cách này đơn giản hơn một PAKE đầy đủ, và có chủ ý như vậy. Nó đủ cho những phiên LAN ngắn. **Không** được thay bằng "TLS nhưng tắt kiểm tra certificate" — làm vậy là không bảo vệ được gì cả, và đó chính là cái bẫy cần tránh.

---

## 10. Tích hợp thư viện ảnh

Engine không bao giờ gọi API nền tảng trực tiếp. Nó nói chuyện với `PhotoLibraryProvider`:

```
phía đọc:  enumerate() → stream AssetDescriptor
           openOriginal(assetId) → stream byte
phía ghi:  createAsset(descriptor, fileURL) → assetId
```

**10.1 iOS đọc (bên gửi)**

- PhotoKit: fetch `PHAsset` theo trang, sắp xếp theo ngày tạo. Không bao giờ nạp cả thư viện vào bộ nhớ.
- Lấy byte gốc qua `PHAssetResource` + `PHAssetResourceManager.requestData` (dạng stream), không đi đường UIImage vì đường đó sẽ nén lại ảnh.
- `PHAsset.localIdentifier` chỉ có ý nghĩa trên máy đó và đổi sau khi cài lại app. Chỉ dùng làm khoá cache nội bộ, tuyệt đối không dùng làm định danh liên máy (§11).
- Cần khai `NSPhotoLibraryUsageDescription`.

**10.2 iOS ghi (bên nhận)**

- `PHPhotoLibrary.performChanges` với `PHAssetCreationRequest.addResource(with: .photo, fileURL:)`.
- File phải được ghi xong và verify xong trước khi gọi. Không bao giờ đưa file dở cho Photos.
- Ảnh vào thư viện chính (Recents), đồng thời được thêm vào album PhotoSync do app tạo.
- Cần khai `NSPhotoLibraryAddUsageDescription`.

**10.3 Android đọc (bên gửi)**

- `MediaStore.Images` / `MediaStore.Video` qua `ContentResolver`, đọc theo trang.
- Quyền: `READ_MEDIA_IMAGES` + `READ_MEDIA_VIDEO` từ API 33, `READ_EXTERNAL_STORAGE` với bản thấp hơn.

**10.4 Android ghi (bên nhận)**

- Insert vào MediaStore với `IS_PENDING = 1`, stream byte vào URI trả về, verify, rồi mới xoá cờ `IS_PENDING`. App chết giữa lúc truyền sẽ để lại một dòng pending vô hình, chứ không để lại ảnh lỗi trong gallery.
- `RELATIVE_PATH = Pictures/PhotoSync` cho ảnh, `Movies/PhotoSync` (hoặc `DCIM/PhotoSync`) cho video.
- Từ API 29, ghi vào bản ghi do chính app insert thì không cần quyền storage.

---

## 11. Định danh ảnh

Tên file không phải định danh. `IMG_1234.HEIC` tồn tại trên mọi chiếc iPhone trên đời.

Hai tầng, thiết kế để tránh phải hash toàn bộ thư viện:

**Khoá nhanh (quick key)** — tính rất rẻ, dùng để quyết định bên nhận đã có ảnh đó chưa:

```
quick_hash = SHA256( kích thước file ‖ 64 KB đầu ‖ 64 KB cuối )
```

Đọc 128 KB thay vì cả file. Với thư viện 18 GB, đây là khác biệt giữa vài giây và vài chục phút.

**Hash đầy đủ** — `SHA256` trên toàn bộ file, tính **dần trong lúc byte đang chạy qua đường truyền**, ở cả hai đầu. Không bao giờ có thêm một lượt đọc file riêng.

Nói gọn: khoá nhanh trả lời "có cần cái này không?", hash đầy đủ trả lời "nó đến nguyên vẹn chưa?".

Các trường descriptor gửi qua đường truyền:

```
quick_hash, size, media_type, mime, created_at, modified_at,
width, height, duration, display_name, resource_group_id
```

---

## 12. Sync tăng dần

Bên nhận giữ một index `quick_hash` của mọi thứ nó từng nhận. Bên gửi hỏi theo lô.

```
bên gửi                                  bên nhận
  │  HAVE_QUERY [500 quick_hash]   ─────►│
  │◄──── MISSING [phần nó chưa có]       │
  │  chỉ truyền phần đó                  │
```

- Lô 500 cái một, hỏi song song với việc truyền để đường mạng không bao giờ rỗi.
- Bên gửi cũng giữ `sent_log(peer_device_id, quick_hash)` và bỏ qua luôn việc hỏi với những cái đã xác nhận gửi thành công cho máy đó — sync lại một thư viện không đổi gần như không tốn traffic.
- Index của bên nhận là căn cứ duy nhất. Nếu người dùng đã xoá ảnh trên máy nhận thì ảnh đó sẽ được gửi lại. Với MVP, đây là hành vi đúng và an toàn.

Lần sync thứ hai của thư viện vừa thêm 100 ảnh sẽ truyền đúng 100 ảnh.

---

## 13. Truyền theo chunk, tiếp tục, kiểm tra toàn vẹn

```
Một video 8 GB
├── chunk 0   ✓ 4 MB
├── chunk 1   ✓
├── ...
└── chunk N   chờ
```

- Chunk cố định 4 MB. Trong một file thì tuần tự, nhờ vậy việc tiếp tục chỉ cần một con số offset.
- Byte nhận được ghi vào file tạm riêng của app (Android: chính URI đang pending). Không bao giờ ghi vào thư viện đang hiển thị.
- Khi nối lại, bên nhận báo `bytes_received` của từng file đang dở, bên gửi tiếp tục từ offset đó. Video 8 GB đứt ở 6,4 GB sẽ tiếp tục từ 6,4 GB.
- Ở chunk cuối, hai bên so `SHA256` đã tính dần. Lệch thì bỏ, truyền lại cả file (tối đa 3 lần), sau đó đánh dấu `FAILED`.
- Chỉ sau khi hash khớp thì file mới được đưa vào Photos / MediaStore. Không tồn tại con đường nào để một file dở hoặc file lỗi trở thành ảnh trong gallery.

**Máy trạng thái** (lưu xuống đĩa, sống qua việc app bị kill):

```
DISCOVERED → QUEUED → TRANSFERRING → VERIFYING → COMMITTED
                          │
                          ├─► FAILED → RETRY → TRANSFERRING
                          ├─► SKIPPED_ALREADY_PRESENT
                          └─► CANCELLED
```

---

## 14. Trạng thái lưu bền (SQLite)

Mỗi máy một database. Không server, không Redis, không Postgres.

```sql
-- phía gửi
local_asset(id, platform_asset_id, quick_hash, full_hash, size,
            media_type, mime, created_at, modified_at,
            resource_group_id, scanned_at)

peer(id, name, platform, cert_fingerprint, last_seen_at)

sent_log(peer_id, quick_hash, sent_at, PRIMARY KEY(peer_id, quick_hash))

transfer(id, session_id, peer_id, local_asset_id, state,
         bytes_transferred, total_bytes, retry_count,
         error_code, created_at, updated_at)

-- phía nhận
received_asset(quick_hash PRIMARY KEY, full_hash, size,
               platform_asset_id, received_at, peer_id)

session(id, role, peer_id, started_at, finished_at,
        items_total, items_done, bytes_total, bytes_done)
```

Database chỉ chứa danh mục và trạng thái. Không bao giờ chứa dữ liệu ảnh/video.

Khi mở app: nạp các transfer chưa xong từ SQLite và mời người dùng tiếp tục (§19).

---

## 15. Loại media, metadata, Live Photos

- Ảnh: JPEG, HEIC/HEIF, PNG, WebP. Video: MP4, MOV, HEVC.
- **Truyền đúng byte gốc, không sửa gì.** Không nén lại, không resize, không mux lại. EXIF, GPS, hướng ảnh, thông tin máy ảnh, ngày tạo đều được giữ vì bản thân file được giữ nguyên.
- Đặt ngày tạo của ảnh ở máy đích theo `created_at` của nguồn, để timeline bên nhận không bị dồn hết vào "hôm nay".
- **Live Photos** là một ảnh logic gồm hai resource dùng chung `resource_group_id`. Định dạng đường truyền mang thông tin nhóm resource ngay từ đầu để sau này không phải viết lại, nhưng việc ghép lại thành Live Photo thật trên iOS (`PHAssetCreationRequest` với `.photo` + `.pairedVideo`) để pha 2. Trong MVP, một Live Photo sẽ đến dưới dạng một ảnh tĩnh và một video ngắn.

---

## 16. Kiến trúc

```
┌──────────────────────────────────┐
│  Tầng UI  (tab Gửi / tab Nhận)   │
├──────────────────────────────────┤
│  Session Orchestrator            │
├──────────────────────────────────┤
│  Sync Engine                     │  ← độc lập nền tảng, test được bằng unit test
│  định danh · so sánh · queue     │
├──────────────────────────────────┤
│  Providers (interface)           │
│  PhotoLibrary · Storage          │
│  Discovery   · Transport         │
└──────────────────────────────────┘
```

Sync Engine không được biết nó đang chạy trên hệ điều hành nào. Nó chỉ biết: Asset, Manifest, Transfer, State, Session.

Mỗi provider cần một bản giả (fake) chạy trong bộ nhớ, để engine test được đầu-cuối mà không cần điện thoại, không cần mạng, không cần thư viện ảnh. Làm mấy bản giả này trước.

---

## 17. Giao thức đường truyền

```
CONNECT
HELLO              phiên bản, tên máy, nền tảng
AUTHENTICATE       HMAC ràng buộc theo mã, cả hai chiều
SESSION_BEGIN      vai trò, số lượng dự kiến, số byte dự kiến
HAVE_QUERY         một lô quick_hash
HAVE_RESPONSE      phần còn thiếu
ASSET_BEGIN        descriptor, tổng kích thước, offset tiếp tục
CHUNK              payload 4 MB
ASSET_END          hash đầy đủ
ASSET_ACK          đã lưu | hash lệch | lỗi
SESSION_END        tổng kết
```

Có version ngay từ byte đầu tiên. Lệch major version thì từ chối kèm câu thông báo người thường đọc được, không phải crash.

---

## 18. Đặc tả UI

Phần phức tạp nằm hết trong sync engine, không bao giờ hiện ra màn hình. Lấy cảm hứng từ LocalSend và AirDrop, không phải từ Google Photos hay dashboard của Immich.

Không đăng nhập. Không tài khoản. Không cấu hình server. Không một từ kỹ thuật nào trên giao diện.

**18.1 Trang chính — hai tab, hết**

```
┌─────────────────────────────┐
│                             │
│        PhotoSync            │
│                             │
│    12.431 ảnh               │
│     1.203 video             │
│                             │
│  ┌──────────┬──────────┐    │
│  │   Gửi    │   Nhận   │    │
│  └──────────┴──────────┘    │
│                             │
│   Nhập mã đang hiện trên     │
│   máy kia                    │
│                             │
│      ┌───────────────┐       │
│      │  _ _ _  _ _ _ │       │
│      └───────────────┘       │
│                             │
│   Gồm cả video        [bật] │
│                             │
│         [ Sync ]             │
│                             │
└─────────────────────────────┘
```

Mặc định là chọn **toàn bộ thư viện**. Luồng chính không có bộ chọn file. "Gồm cả video" là bộ lọc duy nhất trong MVP.

**18.2 Tab Nhận** — xem §6.

**18.3 Đang chạy**

```
┌─────────────────────────────┐
│                             │
│      Đang đồng bộ...        │
│                             │
│           1.284             │
│         / 12.431            │
│                             │
│     ████████████░░░░        │
│                             │
│        235 MB/s             │
│      2,1 GB / 18,4 GB       │
│                             │
│        [ Tạm dừng ]          │
│                             │
└─────────────────────────────┘
```

Giữ màn hình sáng suốt phiên và nói cho người dùng biết một lần. Không bao giờ hiện số thứ tự chunk, hash, trạng thái socket, ID queue hay SQL.

**18.4 Xong**

```
┌─────────────────────────────┐
│                             │
│             ✓               │
│                             │
│       Đồng bộ xong          │
│                             │
│       12.431 mục            │
│      18,4 GB đã truyền      │
│                             │
│   Đã lưu vào Photos của bạn  │
│                             │
│         [ Xong ]             │
│                             │
└─────────────────────────────┘
```

**18.5 Không tìm thấy**

```
│    Không tìm thấy mã đó      │
│                              │
│   Kiểm tra xem hai máy có     │
│   cùng Wi-Fi không.           │
│                              │
│   [ Thử lại ]  [ Trợ giúp ]  │
```

Trợ giúp giải thích cách dùng hotspot bằng lời lẽ thường.

**18.6 Cách dùng từ**

Nút ghi **Sync**, không ghi "Gửi". Bản chất sản phẩm không phải "gửi cho tôi mấy file này", mà là "làm cho máy kia có thư viện của tôi".

**18.7 Cài đặt** — cố tình để rất ít

```
Nơi lưu
Gồm cả video
Giữ màn hình sáng khi đang sync
Tên thiết bị
Giới thiệu
```

---

## 19. Lỗi

Người thường đọc hiểu, có việc để làm tiếp, và tiếp tục được. Không bao giờ hiện mã lỗi.

Sai:
```
ERR_TRANSFER_CHUNK_HASH_MISMATCH
```

Đúng:
```
Chưa đồng bộ xong.
Kết nối bị ngắt.

Đã xong 8.421 / 12.431 mục.

[ Tiếp tục ]      [ Huỷ ]
```

"Tiếp tục" phải là tiếp tục thật, không phải chạy lại từ đầu. Hai bên nạp lại trạng thái từ SQLite (§14).

Những trường hợp cần câu chữ riêng: mã hết hạn, mã sai, máy nhận hết dung lượng, thiếu quyền, hai máy khác mạng, máy quá nóng hoặc gần hết pin.

---

## 20. Giới hạn về quy mô

Thiết kế cho 10.000 / 100.000 / 500.000 ảnh.

- Không bao giờ nạp cả thư viện vào RAM. Đọc theo trang.
- Stream byte của file; không bao giờ đọc cả video vào bộ nhớ.
- Hỏi manifest theo lô; hỏi song song với việc truyền.
- Queue lưu bền là căn cứ duy nhất, không phải một list trong bộ nhớ.
- UI đọc số tổng, không bao giờ đọc cả bảng transfer.

---

## 21. Phạm vi MVP

Có:

- Vai trò Gửi / Nhận, mỗi phiên một chiều
- Mã ghép 6 số, có hết hạn và giới hạn số lần thử
- Tìm máy bằng mDNS, dự phòng quét subnet, chạy được với hotspot
- Chọn toàn bộ thư viện, cả ảnh và video
- Định danh và chống trùng bằng khoá nhanh, không gửi lại thứ đã có
- Truyền theo chunk 4 MB, tiếp tục theo offset byte
- Verify SHA-256 tính dần, trước khi lưu
- Lưu vào Photos (iOS) / MediaStore (Android) của máy đích
- Queue SQLite lưu bền, sống qua việc restart app
- TLS 1.3 với xác thực ràng buộc theo mã
- UI ở §18 và cách xử lý lỗi ở §19

Không có: những thứ ở §3, cộng thêm các phần hoãn lại: sync hai chiều, đồng bộ xoá, album, mục yêu thích, ghép lại Live Photo, sync nền, desktop.

---

## 22. Tiêu chí thành công

1. iPhone có 10.000 ảnh → Android có 0. Kết quả: Android có 10.000 ảnh, thấy được trong app Gallery, ngày tháng đúng.
2. Chạy lại cặp đó sau khi iPhone thêm 100 ảnh. Kết quả: truyền đúng 100 ảnh.
3. Video 8 GB đang ở 60%, ngắt Wi-Fi, nối lại. Kết quả: tiếp tục quanh mốc 60%, hash cuối khớp.
4. Video 5 GB đó máy nhận đã có. Kết quả: không truyền, đánh dấu đã bỏ qua.
5. Android phát hotspot, iPhone vào, không có internet gì cả. Kết quả: ghép máy và sync đều chạy.
6. Hơn 100.000 ảnh. Kết quả: app vẫn mượt, bộ nhớ phẳng, không nạp cả thư viện.
7. Force-kill app giữa lúc sync, mở lại. Kết quả: mời Tiếp tục và chạy xong.
8. Nhập sai mã 3 lần. Kết quả: máy nhận sinh mã mới, không có phiên nào được thiết lập.

Tiêu chí 3, 5 và 7 là ba tiêu chí quyết định sản phẩm này có thật hay không. Test chúng trên máy thật trước khi làm bất kỳ thứ gì thuộc phần đẹp của UI.

---

## 23. Thứ tự triển khai

1. Model dữ liệu ảnh và schema SQLite
2. Provider giả (thư viện, lưu trữ, truyền) — nhờ đó mọi bước dưới test được mà không cần máy thật
3. Khoá nhanh, so sánh manifest, tính ra tập còn thiếu
4. Máy trạng thái transfer và queue lưu bền
5. Truyền theo chunk, tiếp tục, verify SHA-256 tính dần
6. Transport TCP thật, rồi TLS và xác thực theo mã
7. Tìm máy: mDNS, rồi dự phòng quét subnet
8. Adapter đọc PhotoKit trên iOS
9. Adapter ghi MediaStore trên Android (chứng minh iOS → Android chạy đầu-cuối)
10. Cặp adapter cho chiều ngược lại
11. UI theo §18
12. Test hotspot và test ngắt giữa đường trên máy thật

Đừng bắt đầu bằng UI. Engine chính là sản phẩm.

---

## 24. Những điểm chưa quyết

Cần chốt trước hoặc trong lúc làm bước 8. Mỗi điểm đều có giá phải trả thật.

**24.1 iCloud "Optimize iPhone Storage"** — nếu người dùng bật, ảnh gốc không nằm trên máy; PhotoKit phải tải về, mà tải về thì cần internet. Điều này ngược với cam kết "chạy không cần internet". Lựa chọn: (a) bỏ qua những ảnh không có sẵn trên máy và báo số lượng đã bỏ qua, (b) tải về khi có internet, (c) hỏi người dùng một lần lúc bắt đầu phiên. Khuyến nghị: (a) cho MVP, kèm con số rõ ràng trong phần tổng kết, vì như vậy mới giữ đúng lời hứa offline.

**24.2 HEIC / HEVC khi sang Android** — §15 nói không bao giờ nén lại, nên ảnh HEIC từ iPhone sẽ nằm trên Android dưới dạng HEIC. Android giải mã HEIF từ API 28 và các app gallery hiện đại xử lý được, nhưng vài app bên thứ ba thì không. Lựa chọn: giữ nguyên bản gốc (khuyến nghị), hoặc thêm một tuỳ chọn "chuyển sang JPEG cho dễ tương thích" — chỉ phá luật không-nén-lại khi người dùng chủ động bật.

**24.3 Nơi lưu ở máy nhận** — dồn hết vào một album `PhotoSync`, hay chỉ thả vào thư viện chính? Khuyến nghị: vào thư viện chính, đồng thời thêm vào album PhotoSync, để vừa dễ tìm vừa dễ xoá đi nếu muốn làm lại.

**24.4 Khi tắt màn hình** — iOS sẽ treo app lại. Lựa chọn: yêu cầu để màn hình sáng (đơn giản, trung thực) hoặc cố truyền ở chế độ nền (không đáng tin, tốn rất nhiều công). Khuyến nghị: MVP yêu cầu màn hình sáng và nói rõ điều đó trên UI.

**24.5 Live Photos trong MVP** — để nó đến dưới dạng hai mục riêng, hay giữ lại tới pha 2 để không làm rối thư viện bên nhận? Khuyến nghị: cứ truyền cả hai resource, nhóm chúng lại trong database, tạm chấp nhận nhìn thấy hai mục.
