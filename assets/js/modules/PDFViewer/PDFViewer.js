export default class PDFViewer {
    constructor(container, pdfPath) {
        this.container = container;
        this.pdfPath = pdfPath;
        this.init();
    }

    init() {
        // Create an object element to display the PDF
        const pdfObject = document.createElement('object');
        pdfObject.className = "pdfviewer container";
        pdfObject.data = this.pdfPath;
        pdfObject.type = 'application/pdf';
        pdfObject.width = '100%';
        pdfObject.height = '100%';
        
        // Fallback content in case the PDF cannot be displayed
        const fallbackMessage = document.createElement('p');
        fallbackMessage.innerHTML = `It appears you don't have a PDF plugin for this browser. You can <a href="${this.pdfPath}">click here to download the PDF file.</a>`;
        pdfObject.appendChild(fallbackMessage);

        this.container.appendChild(pdfObject);
    }
}
