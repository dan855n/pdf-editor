use image::RgbaImage;
use pdf_viewer::load_pdfium_bindings;
use pdfium_render::prelude::*;

fn main() {
    let pdfium = Pdfium::new(load_pdfium_bindings());
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
    let mut obj =
        PdfPageImageObject::new_with_size(&doc, &red, PdfPoints::new(100.0), PdfPoints::new(50.0))
            .unwrap();
    obj.translate(PdfPoints::new(60.0), PdfPoints::new(650.0))
        .unwrap();
    page.objects_mut().add_image_object(obj).unwrap();
    doc.save_to_file("/tmp/crop_ui_test.pdf").unwrap();
    println!("wrote /tmp/crop_ui_test.pdf");
}
