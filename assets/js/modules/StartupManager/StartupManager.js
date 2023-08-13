import { WindowFactory } from "/assets/js/factories/WindowFactory.js";
import { Terminal } from "/assets/js/modules/terminal/Terminal.js";
import PDFViewer from "/assets/js/modules/PDFViewer/PDFViewer.js"; // Adjust the path accordingly


export default class StartupManager {
    constructor(observable, interactiveWindows) {
        this.observable = observable;
        this.interactiveWindows = interactiveWindows;
    }

    startTerminal() {
        const terminalContainer = WindowFactory.create('Terminal');
        this.interactiveWindows.addWindow(terminalContainer);
        new Terminal(terminalContainer, this.observable, this.interactiveWindows);
        return terminalContainer;
    }

    startPDFViewer(path) {
        const pdfContainer = WindowFactory.create('PDFViewer');
        this.interactiveWindows.addWindow(pdfContainer);
        new PDFViewer(pdfContainer, path);  // Adjust the path to your resume accordingly
        return pdfContainer;
    }
}
