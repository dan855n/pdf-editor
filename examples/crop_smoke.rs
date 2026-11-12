//! Isolated crop smoke test: crop a known red rectangle and inspect the pixels.
use image::RgbaImage;
use pdf_viewer::{OpenDocument, load_pdfium_bindings};
use pdfium_render::prelude::*;

fn main() {
    let pdfium: &'static Pdfium = Box::leak(Box::new(Pdfium::new(load_pdfium_bindings())));
    let path = std::env::temp_dir().join("pdf-editor-crop-smoke.pdf");

    {
        let mut doc = pdfium.create_new_pdf().unwrap();
        let mut page = doc
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .unwrap();
        let red = image::DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            40,
            20,
            image::Rgba([255, 0, 0, 255]),
        ));
        let obj = PdfPageImageObject::new_with_size(
            &doc,
            &red,
            PdfPoints::new(100.0),
            PdfPoints::new(50.0),
        )
        .unwrap();
        page.objects_mut().add_image_object(obj).unwrap();
        doc.save_to_file(&path).unwrap();
    }

    let mut open = OpenDocument::load(pdfium, &path).unwrap();
    let object_index = open
        .doc
        .pages()
        .get(0)
        .unwrap()
        .objects()
        .iter()
        .position(|o| o.as_image_object().is_some())
        .unwrap();

    open.crop_image_object(
        0,
        object_index,
        PdfRect::new_from_values(20.0, 20.0, 70.0, 70.0),
    )
    .unwrap();

    let page = open.doc.pages().get(0).unwrap();
    let obj = page.objects().get(object_index).unwrap();
    let img = obj.as_image_object().unwrap();
    let bounds = img.bounds().unwrap().to_rect();
    println!(
        "bounds after crop: l={} b={} r={} t={}",
        bounds.left().value,
        bounds.bottom().value,
        bounds.right().value,
        bounds.top().value
    );
    let raw = img.get_raw_image().unwrap();
    println!("raw dims: {}x{}", raw.width(), raw.height());
    let rgba = raw.to_rgba8();
    let (w, h) = rgba.dimensions();
    // Center of the cropped image should be red if crop captured the image.
    let p = rgba.get_pixel(w / 2, h / 2);
    println!("center pixel: {:?}", p);
    assert!(
        p[0] > 200 && p[1] < 60 && p[2] < 60,
        "center should be red, got {:?}",
        p
    );
    let _ = std::fs::remove_file(&path);
    println!("crop smoke ok");
}
