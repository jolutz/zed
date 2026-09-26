use fs::FakeFs;
use gpui::TestAppContext;
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::rel_path::rel_path;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
    });
}

#[gpui::test]
async fn test_media_reload_local_audio(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(util::path!("/media"), json!({ "audio.wav": "first" }))
        .await;
    let project = Project::test(fs.clone(), [util::path!("/media").as_ref()], cx).await;
    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let audio = project
        .update(cx, |project, cx| {
            project.open_audio((worktree_id, rel_path("audio.wav")), cx)
        })
        .await
        .unwrap();
    fs.insert_file(util::path!("/media/audio.wav"), b"second".to_vec())
        .await;
    cx.run_until_parked();
    audio.read_with(cx, |audio, _| assert_eq!(audio.bytes.as_slice(), b"second"));
}

#[gpui::test]
async fn test_media_reload_local_image_metadata(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(util::path!("/media"), json!({})).await;
    let png = |width, height| {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    };
    fs.insert_file(util::path!("/media/image.png"), png(2, 3))
        .await;
    let project = Project::test(fs.clone(), [util::path!("/media").as_ref()], cx).await;
    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let image = project
        .update(cx, |project, cx| {
            project.open_image((worktree_id, rel_path("image.png")), cx)
        })
        .await
        .unwrap();
    fs.insert_file(util::path!("/media/image.png"), png(4, 5))
        .await;
    cx.run_until_parked();
    image.read_with(cx, |image, _| {
        let metadata = image.image_metadata.unwrap();
        assert_eq!((metadata.width, metadata.height), (4, 5));
    });
}
