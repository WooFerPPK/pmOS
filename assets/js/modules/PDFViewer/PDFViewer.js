import HTMLLoader from '../HTMLLoader/HTMLLoader.js';

export default class PDFViewer {
    constructor(container, pdfPath, htmlPath, observable) {
        this.observable = observable;

        this.observable.subscribe('windowShutdown', this);

        this.container = container;
        this.pdfPath = pdfPath;

        if (this.isIOS()) {
            const htmlLoader = new HTMLLoader(container, htmlPath, this.observable);
            htmlLoader.load();
        } else {
            this.init();
        }
    }

    update(message) {
        if (message === 'shutdown') {
            this.observable.notify('windowClosed', { source: 'PDFViewer', message: this.container });
        }
    }

    isIOS() {
        return ['iPad Simulator', 'iPhone Simulator', 'iPod Simulator', 'iPad', 'iPhone', 'iPod'].includes(navigator.platform) || (navigator.userAgent.includes('Mac') && 'ontouchend' in document);
    }

    init() {
        const pdfObject = document.createElement('object');
        pdfObject.className = 'pdfviewer container';
        pdfObject.data = this.pdfPath;
        pdfObject.type = 'application/pdf';
        pdfObject.width = '100%';
        pdfObject.height = '100%';
        
        // Fallback content in case the PDF cannot be displayed
        const fallbackMessage = document.createElement('p');
        fallbackMessage.className = 'message'
        fallbackMessage.innerHTML = `It appears you don't have a PDF plugin for this browser. You can <a href="${this.pdfPath}">click here to download the PDF file.</a>`;
        pdfObject.appendChild(fallbackMessage);

        this.container.appendChild(pdfObject);
    }
}
