import { WindowFactory } from '../../factories/WindowFactory.js';
import { Terminal } from '../Terminal/Terminal.js';
import PDFViewer from '../PDFViewer/PDFViewer.js';
import HTMLLoader from '../HTMLLoader/HTMLLoader.js';
import TemplateLoader from '../TemplateLoader/TemplateLoader.js';


export default class StartupManager {
    constructor(interactiveWindows, observable) {
        this.observable = observable
        this.interactiveWindows = interactiveWindows;
    }

    startTerminal() {
        const terminalContainer = WindowFactory.create('Terminal');
        this.interactiveWindows.addWindow(terminalContainer);
        new Terminal(terminalContainer, this.interactiveWindows, this.observable);
        return terminalContainer;
    }
F
    startPDFViewer(pdfPath, htmlPath, title) {
        const pdfContainer = WindowFactory.create(title);
        this.interactiveWindows.addWindow(pdfContainer);
        new PDFViewer(pdfContainer, pdfPath, htmlPath, this.observable);
        return pdfContainer;
    }

    startHTMLViewer(htmlPath, title) {
        const htmlContainer = WindowFactory.create(title);
        this.interactiveWindows.addWindow(htmlContainer);
        const htmlLoader = new HTMLLoader(htmlContainer, htmlPath, this.observable);
        htmlLoader.load();
        return htmlContainer;
    }

    startOpenAbout() {
        const aboutContainer = WindowFactory.create('About');
        this.interactiveWindows.addWindow(aboutContainer);
        const aboutPage = new TemplateLoader(aboutContainer, this.observable, 'assets/templates/html/About/About.html', 'about');
        aboutPage.open();
        return aboutContainer;
    }

    startOpenPage(pageUrl) {
        window.open(pageUrl, '_blank');
    }
}
