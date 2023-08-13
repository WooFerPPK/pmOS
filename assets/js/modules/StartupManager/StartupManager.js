import { WindowFactory } from "/assets/js/factories/WindowFactory.js";
import { Terminal } from "/assets/js/modules/terminal/Terminal.js";
import PDFViewer from "/assets/js/modules/PDFViewer/PDFViewer.js";
import HTMLLoader from "/assets/js/modules/HTMLLoader/HTMLLoader.js";


export default class StartupManager {
    constructor(interactiveWindows) {
        this.interactiveWindows = interactiveWindows;
    }

    startTerminal() {
        const terminalContainer = WindowFactory.create('Terminal');
        this.interactiveWindows.addWindow(terminalContainer);
        new Terminal(terminalContainer, this.interactiveWindows);
        return terminalContainer;
    }

    startPDFViewer(pdfPath, htmlPath) {
        const pdfContainer = WindowFactory.create(`Paul Moscuzza's Resume`);
        this.interactiveWindows.addWindow(pdfContainer);
        new PDFViewer(pdfContainer, pdfPath, htmlPath);
        return pdfContainer;
    }

    startHTMLViewer(htmlPath) {
        const htmlContainer = WindowFactory.create(`Paul Moscuzza's Resume`);
        this.interactiveWindows.addWindow(htmlContainer);
        const htmlLoader = new HTMLLoader(htmlContainer, htmlPath);
        htmlLoader.load();
        return htmlContainer;
    }

    startOpenPage(pageUrl) {
        window.open(pageUrl, '_blank');
    }
}
